//! REST facade over a [`Registry`]: an HTTP gateway in front of a REPE core.
//!
//! This adds a front door; it does not replace one. Public clients get curl,
//! OpenAPI, and edge caching; clients that need the aligned numeric fast path,
//! notify, or sub-millisecond dispatch keep talking REPE to the same registry.
//!
//! The translation is mechanical rather than a redesign, because a REPE query is
//! an RFC 6901 JSON Pointer and a REST resource is a path: the same addressing
//! scheme. Every registered path is a function, and the verbs map onto the two
//! ways of calling one — with a body or without:
//!
//! | HTTP | Registry operation | Safe | Idempotent | Cacheable |
//! | --- | --- | --- | --- | --- |
//! | `GET` / `HEAD` | call the function with no body | yes | yes | yes |
//! | `PUT` / `POST` | call the function with the body as arguments | no | no | no |
//!
//! `PUT` and `POST` are aliases. The registry stores no values, so there is no
//! assignment for `PUT` to mean instead of a call, and neither verb is
//! idempotent because a call is not. A `GET` is treated as safe on the
//! understanding that a bodiless call is a read; a handler that mutates on an
//! empty body is breaking that contract, not the gateway.
//!
//! **This gateway does not authenticate anyone.** See [`RestConfig`] for what it
//! can and cannot bound, and put identity in front of it as you would TLS.
//!
//! The guide covers the caching rules, content negotiation, path mapping, and
//! the error model in full: <https://repe-org.github.io/repe-rs/rest/>.
//!
//! ```no_run
//! use repe::structs::RequestBody;
//! use repe::{Registry, rest::RestGateway};
//! use std::sync::Arc;
//! use std::sync::atomic::{AtomicI64, Ordering};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let registry = Arc::new(Registry::new());
//!
//! // Every registered path is a function, so a stateful endpoint is one that
//! // owns its state: a body sets the counter, no body reads it.
//! let counter = Arc::new(AtomicI64::new(0));
//! registry.register_function("/counter", move |params: Option<RequestBody<'_>>| {
//!     if let Some(body) = params {
//!         let next: i64 = body
//!             .read("/counter")
//!             .map_err(|e| (repe::ErrorCode::InvalidBody, e.to_string()))?;
//!         counter.store(next, Ordering::SeqCst);
//!     }
//!     Ok(counter.load(Ordering::SeqCst))
//! })?;
//!
//! let gateway = RestGateway::new("/api/v1", Arc::clone(&registry));
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
//! 42
//! ```

use crate::constants::{BodyFormat, ErrorCode};
use crate::registry::{Registry, RegistryError};
use crate::structs::{RequestBody, ResponseBody};
use std::sync::Arc;
use std::time::Duration;

/// `application/json`, the default representation on both legs.
pub const MEDIA_JSON: &str = "application/json";
/// `application/x-beve`, the BEVE representation.
pub const MEDIA_BEVE: &str = "application/x-beve";
/// `application/problem+json`, RFC 9457 problem details, used for every failure.
pub const MEDIA_PROBLEM: &str = "application/problem+json";

/// The `Allow` set for a registered path.
///
/// `PUT` and `POST` are both here because both call the function. The registry
/// no longer stores values, so there is no second kind of target to reserve a
/// verb for, and refusing one of the two would only make a caller guess which
/// alias this deployment happens to accept.
const ALLOW_CALL: &str = "GET, HEAD, PUT, POST, OPTIONS";
/// The `Allow` set when policy forbids mutation.
const ALLOW_READ_ONLY: &str = "GET, HEAD, OPTIONS";

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

    /// The REPE body format that carries this representation.
    fn body_format(self) -> BodyFormat {
        match self {
            Repr::Json => BodyFormat::Json,
            Repr::Beve => BodyFormat::Beve,
        }
    }

    /// The representation a settled response format names.
    ///
    /// [`ResponseBody`] only ever settles on JSON or BEVE, so the fallback is
    /// unreachable rather than a guess: it exists because `BodyFormat` is
    /// `#[non_exhaustive]`, and JSON is the representation this gateway
    /// documents as its default.
    fn from_body_format(format: BodyFormat) -> Self {
        match format {
            BodyFormat::Beve => Repr::Beve,
            _ => Repr::Json,
        }
    }
}

/// Gateway policy: what is reachable, how large, how cacheable, and how errors
/// are graded.
///
/// # This gateway has no authentication
///
/// Nothing here authenticates or authorizes a caller, and the defaults below are
/// chosen on the assumption that something in front of it does. Put it behind a
/// terminator that handles identity — an ingress, an API gateway, a sidecar —
/// exactly as you would put it behind one for TLS. The knobs here reduce blast
/// radius; they are not an access-control system.
#[derive(Debug, Clone)]
pub struct RestConfig {
    /// Reject a request body larger than this with `413`. Defaults to 1 MiB.
    ///
    /// This bounds the *facade*, not the protocol. A REST body is buffered whole
    /// before it can be decoded, so an unbounded limit here is an unbounded
    /// allocation driven by an anonymous caller.
    /// Bulk payloads belong on the REPE side, which streams.
    pub max_body_bytes: usize,
    /// `Cache-Control` for successful reads. Defaults to `no-cache`:
    /// revalidate every time, which keeps `ETag` and `304` working while never
    /// serving stale state. Set a `max-age` where the data tolerates it — that
    /// is where the edge-caching win actually comes from.
    pub cache_control: Option<String>,
    /// The status for an [`ErrorCode::ApplicationErrorBase`] handler failure.
    /// Defaults to `500`.
    ///
    /// The gateway cannot know whether a given handler's error means "you sent
    /// the wrong thing" (`400`/`422`) or "something here is broken" (`500`), and
    /// guessing 4xx would tell clients not to retry failures that a retry would
    /// fix. `500` is the honest default; a deployment that knows what its
    /// handlers mean should say so here.
    pub application_error_status: u16,
    /// Serve reads only: answer every `PUT` and `POST` with `405`. Defaults to
    /// `false`.
    ///
    /// Reads are the traffic this facade exists for — they are the cacheable,
    /// safe, CDN-frontable half. A deployment that only needs to publish state
    /// should say so here rather than rely on an upstream to filter methods,
    /// because the mutation surface is otherwise reachable by anyone who can
    /// reach the port.
    pub read_only: bool,
    /// Accept BEVE request *bodies*. Defaults to `true`.
    ///
    /// This was off by default while the previous BEVE decoder had no recursion
    /// limit: a few kilobytes of nested array tags overflowed the thread stack,
    /// and a Rust stack overflow aborts the process rather than unwinding into
    /// the per-connection catch, so one anonymous request took down every other
    /// connection with it. structio bounds nesting at
    /// [`structio::beve::MAX_DEPTH`] and reports the refusal as an ordinary
    /// error, which this gateway answers with a `400` like any other malformed
    /// body. BEVE responses were never affected — encoding is driven by the
    /// server's own data.
    ///
    /// Still a knob, because content negotiation is policy: a gateway that
    /// publishes a JSON-only contract can turn it off and refuse the media type
    /// outright rather than accept a representation it does not document.
    pub accept_beve_bodies: bool,
    /// Maximum connections served concurrently by [`serve`](RestGateway::serve).
    /// Defaults to 1024.
    ///
    /// Without a cap, every accepted socket becomes an unbounded task holding a
    /// file descriptor, so idle connections that never send a byte march the
    /// process to its descriptor limit. Past the cap, `serve` stops accepting
    /// until a connection finishes, which leaves the backlog to absorb the
    /// spike instead of the descriptor table.
    pub max_connections: usize,
    /// Maximum time [`serve`](RestGateway::serve) gives one request, from the
    /// moment its head is parsed to the moment a response is produced. Answers
    /// `408` and closes the connection on expiry. Defaults to 30 seconds;
    /// `None` disables it.
    ///
    /// A header-read timeout alone does not bound a request. A client that
    /// sends a complete, well-formed head promising `Content-Length: 100` and
    /// then sends no body holds its connection — and its slot under
    /// [`max_connections`](Self::max_connections) — for as long as it likes.
    /// A few hundred such sockets, costing an attacker almost nothing, stop the
    /// gateway from accepting anything at all. This is what bounds the body
    /// read, which is the half of a request the head timeout has already
    /// stopped watching.
    pub request_timeout: Option<Duration>,
}

impl Default for RestConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: 1024 * 1024,
            cache_control: Some("no-cache".to_string()),
            application_error_status: 500,
            read_only: false,
            accept_beve_bodies: true,
            max_connections: 1024,
            request_timeout: Some(Duration::from_secs(30)),
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
        let problem = Problem {
            kind: "about:blank",
            title: reason_phrase(status),
            status,
            detail: detail.into(),
            repe_code,
        };
        Self {
            content_type: Some(MEDIA_PROBLEM),
            // RFC 9111 §4.2.2 makes 404 and 405 heuristically cacheable. A
            // cached 405 keeps its stale `Allow` after the target becomes a
            // function, leaving the resource unreachable through its only
            // correct verb; a cached 404 outlives the path being registered.
            cache_control: Some("no-store".to_string()),
            repe_error_code: repe_code,
            // Encoding a `Problem` cannot fail: structio writes are infallible,
            // and every field here is a plain scalar or string.
            body: structio::json::to_vec_with::<structio::SkipNull, _>(&problem),
            ..Self::empty(status)
        }
    }
}

/// The RFC 9457 problem-details body this gateway sends for every failure.
///
/// `type` is a Rust keyword, so the field is `kind` and the wire name is set
/// explicitly. It is written with [`SkipNull`](structio::SkipNull) so that
/// `repe_code` is *absent* rather than `null` when there is no REPE error
/// behind the failure, which is what RFC 9457 §3.2 asks of an extension member
/// that does not apply.
/// The lifetime is here only because `structio::object!` declares both
/// directions and a `&str` field has to be able to borrow from the input to be
/// readable. This gateway only ever writes a `Problem`; every value it builds
/// is `'static`.
#[derive(Default)]
struct Problem<'a> {
    kind: &'a str,
    title: &'a str,
    status: u16,
    detail: String,
    repe_code: Option<u32>,
}

structio::object!(['de] Problem<'de> {
    "type" => kind,
    title,
    status,
    detail,
    repe_code,
});

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
    /// `mount` is normalized exactly as [`Router::with_registry`] normalizes
    /// its prefix, through the same function: made absolute, stripped of a
    /// trailing separator, and empty for a root mount. `"/api/v1"`,
    /// `"/api/v1/"`, and `"api/v1"` all name the same mount.
    ///
    /// [`Router::with_registry`]: crate::server::Router::with_registry
    pub fn new(mount: &str, registry: Arc<Registry>) -> Self {
        Self::with_config(mount, registry, RestConfig::default())
    }

    /// [`new`](Self::new) with explicit policy.
    pub fn with_config(mount: &str, registry: Arc<Registry>, config: RestConfig) -> Self {
        Self {
            registry,
            mount: crate::registry::normalize_mount(mount),
            config,
        }
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

        // RFC 9110 §9.3.7: `OPTIONS *` asks about the server as a whole rather
        // than a resource, so it never reaches the mount or the pointer mapping.
        if request.method == "OPTIONS" && path == "*" {
            let allow = if self.config.read_only {
                ALLOW_READ_ONLY
            } else {
                ALLOW_CALL
            };
            return RestResponse {
                allow: Some(allow),
                ..RestResponse::empty(204)
            };
        }

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

        match request.method {
            "GET" | "HEAD" => self.read(&pointer, request),
            // `PUT` and `POST` are aliases: both call the function at the
            // pointer, with the request body as its arguments. The registry
            // stores no values, so there is no assignment for `PUT` to mean
            // instead, and the two verbs cannot be told apart by what they do.
            //
            // Neither is idempotent, because a call is not. A caller that wants
            // RFC 9110 §9.2.2 idempotence out of `PUT` is relying on a promise
            // the registry never made; the honest reading is that this gateway
            // exposes calls and nothing else.
            "PUT" | "POST" => self.call(&pointer, request),
            "OPTIONS" => RestResponse {
                allow: Some(self.allow()),
                ..RestResponse::empty(204)
            },
            _ => method_not_allowed(self.allow(), "unsupported method"),
        }
    }

    /// The `Allow` set this gateway reports, narrowed by policy so discovery
    /// says what it will actually do rather than what the registry could in
    /// principle support.
    fn allow(&self) -> &'static str {
        if self.config.read_only {
            ALLOW_READ_ONLY
        } else {
            ALLOW_CALL
        }
    }

    /// `GET` / `HEAD`: call the function at `pointer` with no arguments, plus
    /// validators and conditional handling.
    ///
    /// **The handler runs before a `304` can be answered.** The `ETag` is a
    /// hash of the response bytes, so producing it requires producing them; a
    /// conditional request saves the *transfer*, not the work. That is a real
    /// change from a value store, where the current value could be read and
    /// hashed without invoking anything. A handler that is expensive enough for
    /// this to matter should cache on its own side.
    fn read(&self, pointer: &str, request: RestRequest<'_>) -> RestResponse {
        let (body, media) = match self.invoke(pointer, None, negotiate(request.accept)) {
            Ok(encoded) => encoded,
            Err(err) => return self.problem_for(err),
        };

        let etag = etag_for(&body);
        let cache_control = self.config.cache_control.clone();

        // A matching validator ends the request here — the point of the ETag.
        if let Some(header) = request.if_none_match
            && if_none_match_matches(header, &etag)
        {
            return RestResponse {
                etag: Some(etag),
                cache_control,
                vary: Some("Accept"),
                ..RestResponse::empty(304)
            };
        }

        RestResponse {
            status: 200,
            content_type: Some(media),
            etag: Some(etag),
            cache_control,
            vary: Some("Accept"),
            omit_body: request.method == "HEAD",
            body,
            ..RestResponse::empty(200)
        }
    }

    /// `PUT` / `POST`: call the function at `pointer` with the request body as
    /// its arguments.
    ///
    /// There are no preconditions here. `If-Match` and `If-None-Match` compared
    /// a tag against a stored value, and there is no stored value to compare
    /// against: a call's effect is the handler's business, and a gateway that
    /// evaluated a validator against the *previous call's output* would be
    /// answering `412` on a question nobody asked. A handler that needs
    /// compare-and-swap takes the expected state as an argument, where it is
    /// inside the handler's own lock rather than racing outside it.
    fn call(&self, pointer: &str, request: RestRequest<'_>) -> RestResponse {
        if self.config.read_only {
            return method_not_allowed(ALLOW_READ_ONLY, "this gateway is configured read-only");
        }

        // An empty body is how REPE spells "no arguments", which is what a GET
        // already does. Letting one through here would make PUT and GET the
        // same request under a verb that says otherwise, so it is refused
        // rather than reinterpreted.
        if request.body.is_empty() {
            return RestResponse::problem(
                400,
                "a body is required; send `null` for a call that takes no arguments",
                Some(ErrorCode::InvalidBody as u32),
            );
        }

        let format = match self.body_format(request.content_type) {
            Ok(format) => format,
            Err(detail) => {
                return RestResponse::problem(415, detail, Some(ErrorCode::InvalidBody as u32));
            }
        };

        let params = RequestBody::new(request.body, format);
        let (body, media) = match self.invoke(pointer, Some(params), negotiate(request.accept)) {
            Ok(encoded) => encoded,
            Err(err) => return self.problem_for(err),
        };

        RestResponse {
            content_type: Some(media),
            // `Vary` even though a mutation response is not cacheable today:
            // negotiation selected this representation, RFC 9110 §12.5.5 asks
            // for it whenever that is true, and it stops being a formality the
            // moment anyone gives these responses a freshness directive.
            vary: Some("Accept"),
            // No validator: `ETag` is a read concern, and the body here is what
            // one call returned rather than a resource's representation.
            body,
            ..RestResponse::empty(200)
        }
    }

    /// Call the registry and hand back the encoded response body and its media
    /// type.
    ///
    /// The handler writes straight into the response buffer in the negotiated
    /// format, so nothing is transcoded and no value exists between the
    /// function and the socket. `repr` is what the caller asked for; the format
    /// the handler actually settled on is what names the media type, because a
    /// handler is free to answer in the other one.
    ///
    /// The error is the registry's, not an HTTP response: grading it into a
    /// status belongs to [`problem_for`](Self::problem_for), at the layer that
    /// knows about statuses.
    fn invoke(
        &self,
        pointer: &str,
        params: Option<RequestBody<'_>>,
        repr: Repr,
    ) -> Result<(Vec<u8>, &'static str), RegistryError> {
        let mut buf = Vec::new();
        let mut out = ResponseBody::with_format(&mut buf, repr.body_format());
        let ctx = crate::CallContext::detached(pointer);
        self.registry.call(pointer, params, &ctx, &mut out)?;
        let media = Repr::from_body_format(out.format()).media_type();
        Ok((buf, media))
    }

    /// Pick a request-body format from `Content-Type`. A missing header is
    /// JSON, which is what `curl -d` sends.
    fn body_format(&self, content_type: Option<&str>) -> Result<BodyFormat, &'static str> {
        let Some(content_type) = content_type else {
            return Ok(BodyFormat::Json);
        };
        match bare_media(content_type, ';').as_str() {
            "" | MEDIA_JSON | "text/json" => Ok(BodyFormat::Json),
            // `curl -d` defaults to this and means JSON often enough that
            // rejecting it would be pedantry at the cost of the primary use case.
            "application/x-www-form-urlencoded" => Ok(BodyFormat::Json),
            MEDIA_BEVE | "application/beve" if self.config.accept_beve_bodies => {
                Ok(BodyFormat::Beve)
            }
            MEDIA_BEVE | "application/beve" => Err(
                "BEVE request bodies are disabled; enable `RestConfig::accept_beve_bodies` \
                 or send application/json",
            ),
            _ => Err("unsupported Content-Type; send application/json"),
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
        RestResponse::problem(self.status_for(code), err.to_string(), Some(code as u32))
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
        pointer.push_str(&crate::json_pointer::escape_token(&percent_decode(
            segment,
        )?));
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
        let media = bare_media(entry, ';');
        let mut quality = 1000;
        for param in entry.split(';').skip(1) {
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

/// The bare media type from a header value: everything before the first
/// `delimiter`, trimmed and lowercased.
fn bare_media(value: &str, delimiter: char) -> String {
    value
        .split(delimiter)
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
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

/// The registered reason phrase for a status, from `http`'s own table rather
/// than a local copy that would have to be extended per status emitted.
fn reason_phrase(status: u16) -> &'static str {
    hyper::StatusCode::from_u16(status)
        .ok()
        .and_then(|status| status.canonical_reason())
        .unwrap_or("Error")
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
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use std::convert::Infallible;

impl RestGateway {
    /// Serve this gateway on `listener` until the process ends.
    ///
    /// The accept loop does not give up: a failure that is not about one
    /// connection — descriptor exhaustion being the one that matters — is
    /// retried with a capped backoff rather than surfaced, because a gateway
    /// that stays down after the flood causing it has drained is the outcome
    /// the cap exists to prevent.
    ///
    /// Both HTTP/1.1 and HTTP/2 are served on the one listener via hyper's
    /// protocol-detecting builder: HTTP/2 for a client that arrives with prior
    /// knowledge (or via ALPN behind a TLS terminator), HTTP/1.1 for curl and
    /// for the origin leg behind a CDN. Nothing in the mapping depends on which
    /// one a request arrived over.
    ///
    /// TLS and authentication are deliberately out of scope. A gateway of this
    /// shape belongs behind a terminator that is already doing certificate
    /// rotation, ALPN, and identity; see [`RestConfig`] for what this side can
    /// and cannot bound.
    ///
    /// At most [`RestConfig::max_connections`] connections are served at once.
    /// Past that, `serve` stops accepting until one finishes, so a burst waits
    /// in the listen backlog rather than in the descriptor table. That cap is
    /// only as good as the bound on how long one connection can hold its slot,
    /// which is [`RestConfig::request_timeout`].
    pub async fn serve(self, listener: tokio::net::TcpListener) -> std::io::Result<()> {
        let gateway = Arc::new(self);
        let permits = Arc::new(tokio::sync::Semaphore::new(gateway.config.max_connections));
        let mut backoff = ACCEPT_BACKOFF;

        loop {
            // Taken before `accept`, so a full gateway leaves connections in the
            // backlog instead of accepting sockets it has no capacity to serve.
            // `close()` is never called on this semaphore, so acquiring cannot
            // fail.
            let Ok(permit) = Arc::clone(&permits).acquire_owned().await else {
                return Ok(());
            };

            let stream = match listener.accept().await {
                Ok((stream, _peer)) => {
                    backoff = ACCEPT_BACKOFF;
                    stream
                }
                // The connection died between the backlog and here. Routine.
                Err(err) if is_per_connection_accept_error(&err) => continue,
                Err(_) => {
                    // Everything else is treated as descriptor pressure, which
                    // is what `EMFILE`/`ENFILE` are — and Rust maps both to
                    // `Uncategorized`, so they cannot be matched by kind. Back
                    // off and keep trying rather than counting to a ceiling and
                    // returning: the whole point of surviving a descriptor
                    // flood is to still be serving once the attacker
                    // disconnects, and a loop that gives up after N failures
                    // stays down instead. Held connections drain on their own,
                    // and the backoff caps the spin at one attempt per
                    // `MAX_ACCEPT_BACKOFF`.
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_ACCEPT_BACKOFF);
                    continue;
                }
            };

            let gateway = Arc::clone(&gateway);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                    let gateway = Arc::clone(&gateway);
                    async move {
                        let response = match gateway.config.request_timeout {
                            Some(limit) => tokio::time::timeout(limit, gateway.handle(request))
                                .await
                                .unwrap_or_else(|_| request_timeout_response()),
                            None => gateway.handle(request).await,
                        };
                        Ok::<_, Infallible>(response)
                    }
                });
                let mut builder = auto::Builder::new(TokioExecutor::new());
                // Without a header timeout a client that connects and sends
                // nothing holds its task and its descriptor forever, which is
                // how an idle-connection flood reaches the descriptor limit
                // without ever sending a valid request.
                builder
                    .http1()
                    // The timer is not optional: hyper panics on a timeout set
                    // with no timer installed.
                    .timer(TokioTimer::new())
                    .header_read_timeout(HEADER_READ_TIMEOUT);
                // The connection's own errors are its own: a client that hangs
                // up mid-response is routine, and there is no caller left to
                // report it to.
                let _ = builder
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
                drop(permit);
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

/// How long a client may take to send its request head before the connection is
/// dropped. Bounds slowloris, which is otherwise a descriptor leak.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// First pause after an `accept` failure that is not about one connection, so
/// the retry is a pause rather than a busy loop. Doubles up to
/// [`MAX_ACCEPT_BACKOFF`] while failures continue, and resets on the next
/// successful accept.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// Ceiling for that pause. Bounds the spin under sustained descriptor pressure
/// without ever giving up on the listener, which would leave the gateway down
/// after the condition that caused it has cleared.
const MAX_ACCEPT_BACKOFF: Duration = Duration::from_secs(1);

/// Whether an `accept` failure was about the connection rather than the process.
///
/// Everything else — descriptor exhaustion (`EMFILE`/`ENFILE`), `ENOBUFS`, and
/// anything with no nameable [`std::io::ErrorKind`] — is retried with a backoff
/// instead. Ending the loop there is what lets an unauthenticated flood of idle
/// connections take the gateway down permanently: it stays down after the
/// attacker disconnects. Retrying without a backoff is the opposite failure, a
/// hot loop on a condition that needs time to clear, so the caller pairs this
/// with [`ACCEPT_BACKOFF`] and gives up only after
/// [`MAX_CONSECUTIVE_ACCEPT_FAILURES`] in a row.
fn is_per_connection_accept_error(err: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        err.kind(),
        ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset | ErrorKind::Interrupted
    )
}

/// The answer to a request that outlived [`RestConfig::request_timeout`].
///
/// `408` rather than `503`: the request itself did not arrive in time, which is
/// what the client can act on. `Connection: close` because the body it promised
/// is still unread, so the stream is no longer at a message boundary and the
/// connection cannot be reused.
fn request_timeout_response() -> Response<Full<Bytes>> {
    let mut response = to_hyper(RestResponse::problem(
        408,
        "the request was not completed within the gateway's time limit",
        None,
    ));
    response.headers_mut().insert(
        hyper::header::CONNECTION,
        hyper::header::HeaderValue::from_static("close"),
    );
    response
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
    use std::sync::atomic::{AtomicI64, Ordering};

    // ---- fixtures ----------------------------------------------------------

    #[derive(Default, Debug, PartialEq)]
    struct Operands {
        a: i64,
        b: i64,
    }
    structio::object!(Operands { a, b });

    #[derive(Default, Debug, PartialEq)]
    struct Sum {
        result: i64,
    }
    structio::object!(Sum { result });

    #[derive(Default, Debug, PartialEq)]
    struct Counter {
        counter: i64,
    }
    structio::object!(Counter { counter });

    #[derive(Default, Debug, PartialEq)]
    struct Config {
        verbose: bool,
    }
    structio::object!(Config { verbose });

    #[derive(Default, Debug, PartialEq)]
    struct Label {
        label: String,
    }
    structio::object!(Label { label });

    /// The problem-details body, read back. `Problem` itself borrows, and a
    /// decoded response outlives the buffer it came from, so the test reads
    /// into an owned mirror rather than the type the gateway writes.
    #[derive(Default, Debug, PartialEq)]
    struct ProblemBody {
        kind: String,
        title: String,
        status: u16,
        detail: String,
        repe_code: Option<u32>,
    }
    structio::object!(ProblemBody {
        "type" => kind,
        title,
        status,
        detail,
        repe_code
    });

    /// Every registered path is a function, so the fixture registry is a set of
    /// functions. `/counter` holds state behind one, which is how a value
    /// endpoint is spelled now: the handler owns the state and decides what a
    /// body means, rather than the registry storing a tree the gateway writes
    /// into blind.
    fn gateway_with(config: RestConfig) -> RestGateway {
        let registry = Arc::new(Registry::new());

        let counter = Arc::new(AtomicI64::new(7));
        let read_counter = Arc::clone(&counter);
        registry
            .register_function("/counter", move |params: Option<RequestBody<'_>>| {
                // A body sets the counter; no body reads it. The handler makes
                // that rule, and it is the handler's to make.
                if let Some(body) = params {
                    let next: i64 = body
                        .read("/counter")
                        .map_err(|err| (ErrorCode::InvalidBody, err.to_string()))?;
                    read_counter.store(next, Ordering::SeqCst);
                }
                Ok(read_counter.load(Ordering::SeqCst))
            })
            .unwrap();

        registry
            .register_function("/config", |_: Option<RequestBody<'_>>| {
                Ok(Config { verbose: false })
            })
            .unwrap();
        // A key with a literal `/` in it, reachable only through `~1`. This is
        // what the percent-decode-then-escape order exists to keep addressable.
        registry
            .register_function("/items/a~1b", |_: Option<RequestBody<'_>>| {
                Ok("slashed".to_string())
            })
            .unwrap();
        registry
            .register_function("/tilde/a~0b", |_: Option<RequestBody<'_>>| {
                Ok("tilded".to_string())
            })
            .unwrap();
        registry
            .register_function("/add", |params: Option<RequestBody<'_>>| {
                let Some(body) = params else {
                    return Err((ErrorCode::InvalidBody, "expected an object".into()));
                };
                let operands: Operands = body
                    .read("/add")
                    .map_err(|err| (ErrorCode::InvalidBody, err.to_string()))?;
                Ok(Sum {
                    result: operands.a + operands.b,
                })
            })
            .unwrap();
        registry
            .register_function("/boom", |_: Option<RequestBody<'_>>| {
                Err::<(), _>((ErrorCode::ApplicationErrorBase, "handler said no".into()))
            })
            .unwrap();
        RestGateway::with_config("/api/v1", registry, config)
    }

    fn gateway() -> RestGateway {
        gateway_with(RestConfig::default())
    }

    fn body_as<T: crate::structs::ServableOwned>(response: &RestResponse) -> T {
        structio::from_slice(&response.body).expect("response body is JSON")
    }

    fn counter_of(gateway: &RestGateway) -> i64 {
        body_as(&gateway.respond(RestRequest::new("GET", "/api/v1/counter")))
    }

    // -- the verb mapping ---------------------------------------------------

    #[test]
    fn get_calls_a_function_with_no_arguments() {
        let response = gateway().respond(RestRequest::new("GET", "/api/v1/counter"));
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, Some(MEDIA_JSON));
        assert_eq!(body_as::<i64>(&response), 7);
    }

    #[test]
    fn post_calls_a_function_with_the_body_as_arguments() {
        let response = gateway().respond(
            RestRequest::new("POST", "/api/v1/add").with_body(MEDIA_JSON, br#"{"a":2,"b":3}"#),
        );
        assert_eq!(response.status, 200);
        assert_eq!(body_as::<Sum>(&response), Sum { result: 5 });
    }

    #[test]
    fn put_and_post_are_the_same_call() {
        // The registry stores no values, so there is no assignment for PUT to
        // mean instead of a call, and nothing to tell the two verbs apart by.
        // They are documented aliases; this pins that they stay aliases rather
        // than drifting into a distinction the registry cannot honor.
        let gateway = gateway();
        let via_put = gateway.respond(
            RestRequest::new("PUT", "/api/v1/add").with_body(MEDIA_JSON, br#"{"a":2,"b":3}"#),
        );
        let via_post = gateway.respond(
            RestRequest::new("POST", "/api/v1/add").with_body(MEDIA_JSON, br#"{"a":2,"b":3}"#),
        );
        assert_eq!(via_put.status, 200);
        assert_eq!(via_post.status, 200);
        assert_eq!(via_put.body, via_post.body);
        assert_eq!(via_put.content_type, via_post.content_type);
    }

    #[test]
    fn a_call_with_no_body_is_refused_rather_than_read() {
        // An empty body is how REPE spells "no arguments", which is what GET
        // already sends. Letting one through would make PUT and GET the same
        // request under a verb that says otherwise.
        for method in ["PUT", "POST"] {
            let response = gateway().respond(RestRequest::new(method, "/api/v1/add"));
            assert_eq!(response.status, 400, "{method}");
            assert_eq!(
                response.repe_error_code,
                Some(ErrorCode::InvalidBody as u32),
                "{method}"
            );
        }
    }

    #[test]
    fn a_no_argument_call_is_spelled_with_a_null_body() {
        let registry = Arc::new(Registry::new());
        registry
            .register_function("/ping", |_: Option<RequestBody<'_>>| Ok("pong".to_string()))
            .unwrap();
        let gateway = RestGateway::new("/api/v1", registry);
        let response = gateway
            .respond(RestRequest::new("POST", "/api/v1/ping").with_body(MEDIA_JSON, b"null"));
        assert_eq!(response.status, 200);
        assert_eq!(body_as::<String>(&response), "pong");
    }

    #[test]
    fn options_advertises_the_verbs_the_gateway_supports() {
        let response = gateway().respond(RestRequest::new("OPTIONS", "/api/v1/counter"));
        assert_eq!(response.status, 204);
        assert_eq!(response.allow, Some(ALLOW_CALL));
    }

    #[test]
    fn an_unsupported_method_is_405_with_allow() {
        let response = gateway().respond(RestRequest::new("DELETE", "/api/v1/counter"));
        assert_eq!(response.status, 405);
        assert_eq!(response.allow, Some(ALLOW_CALL));
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
    fn a_call_carries_neither() {
        let response = gateway().respond(
            RestRequest::new("POST", "/api/v1/add").with_body(MEDIA_JSON, br#"{"a":1,"b":1}"#),
        );
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
    fn the_handler_runs_even_when_the_answer_is_304() {
        // The ETag is a hash of the response bytes, so producing it requires
        // producing them. A conditional request saves the transfer, not the
        // work — which is a real change from a value store, and the reason
        // `read`'s docs say so.
        let calls = Arc::new(AtomicI64::new(0));
        let counted = Arc::clone(&calls);
        let registry = Arc::new(Registry::new());
        registry
            .register_function("/probe", move |_: Option<RequestBody<'_>>| {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(1i64)
            })
            .unwrap();
        let gateway = RestGateway::new("/api/v1", registry);

        let first = gateway.respond(RestRequest::new("GET", "/api/v1/probe"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let tag = first.etag.unwrap();
        let second =
            gateway.respond(RestRequest::new("GET", "/api/v1/probe").with_if_none_match(&tag));
        assert_eq!(second.status, 304);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the 304 was decided from the handler's own output"
        );
    }

    #[test]
    fn a_stale_validator_answers_the_full_body() {
        let gateway = gateway();
        let response = gateway.respond(
            RestRequest::new("GET", "/api/v1/counter").with_if_none_match("\"0000000000000000\""),
        );
        assert_eq!(response.status, 200);
        assert_eq!(body_as::<i64>(&response), 7);
    }

    #[test]
    fn the_validator_changes_when_the_answer_does() {
        let gateway = gateway();
        let before = gateway.respond(RestRequest::new("GET", "/api/v1/counter"));
        gateway.respond(RestRequest::new("PUT", "/api/v1/counter").with_body(MEDIA_JSON, b"8"));
        let after = gateway.respond(RestRequest::new("GET", "/api/v1/counter"));
        assert_ne!(before.etag, after.etag);

        // And the stale validator no longer short-circuits.
        let conditional = gateway.respond(
            RestRequest::new("GET", "/api/v1/counter").with_if_none_match(&before.etag.unwrap()),
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
        let as_beve =
            gateway.respond(RestRequest::new("GET", "/api/v1/config").with_accept(MEDIA_BEVE));
        assert_ne!(as_json.etag, as_beve.etag);
        assert_eq!(as_json.vary, Some("Accept"));
        assert_eq!(as_beve.vary, Some("Accept"));
    }

    // -- content negotiation ------------------------------------------------

    #[test]
    fn beve_is_served_on_request_and_decodes_to_the_same_value() {
        // The handler writes into the response buffer in the negotiated format,
        // so there is no transcode step between it and the socket.
        let response =
            gateway().respond(RestRequest::new("GET", "/api/v1/config").with_accept(MEDIA_BEVE));
        assert_eq!(response.content_type, Some(MEDIA_BEVE));
        let decoded: Config = structio::from_beve(&response.body).unwrap();
        assert_eq!(decoded, Config { verbose: false });
    }

    #[test]
    fn a_beve_request_body_is_accepted_by_default() {
        let body = structio::to_beve(&Operands { a: 20, b: 22 });
        let response =
            gateway().respond(RestRequest::new("POST", "/api/v1/add").with_body(MEDIA_BEVE, &body));
        assert_eq!(response.status, 200);
        assert_eq!(body_as::<Sum>(&response), Sum { result: 42 });
    }

    #[test]
    fn the_request_and_response_formats_are_independent() {
        // A BEVE request answered in JSON, and a JSON request answered in BEVE.
        // The header declares each leg separately, and the gateway honors both.
        let gateway = gateway();

        let beve_in = structio::to_beve(&Operands { a: 1, b: 2 });
        let json_out = gateway.respond(
            RestRequest::new("POST", "/api/v1/add")
                .with_body(MEDIA_BEVE, &beve_in)
                .with_accept(MEDIA_JSON),
        );
        assert_eq!(json_out.content_type, Some(MEDIA_JSON));
        assert_eq!(body_as::<Sum>(&json_out), Sum { result: 3 });

        let beve_out = gateway.respond(
            RestRequest::new("POST", "/api/v1/add")
                .with_body(MEDIA_JSON, br#"{"a":1,"b":2}"#)
                .with_accept(MEDIA_BEVE),
        );
        assert_eq!(beve_out.content_type, Some(MEDIA_BEVE));
        assert_eq!(
            structio::from_beve::<Sum>(&beve_out.body).unwrap(),
            Sum { result: 3 }
        );
    }

    #[test]
    fn quality_values_are_honored() {
        let gateway = gateway();
        let cases = [
            ("application/json", MEDIA_JSON),
            ("application/x-beve", MEDIA_BEVE),
            ("*/*", MEDIA_JSON),
            (
                "application/json;q=0.1, application/x-beve;q=0.9",
                MEDIA_BEVE,
            ),
            (
                "application/json;q=0.9, application/x-beve;q=0.1",
                MEDIA_JSON,
            ),
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
        assert_eq!(counter_of(&gateway), 99);
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
        assert_eq!(body_as::<String>(&response), "slashed");
    }

    #[test]
    fn a_literal_tilde_in_a_segment_is_escaped_not_interpreted() {
        let response = gateway().respond(RestRequest::new("GET", "/api/v1/tilde/a~b"));
        assert_eq!(response.status, 200);
        assert_eq!(body_as::<String>(&response), "tilded");
    }

    #[test]
    fn a_trailing_slash_is_the_same_resource() {
        let gateway = gateway();
        assert_eq!(
            gateway
                .respond(RestRequest::new("GET", "/api/v1/counter/"))
                .body,
            gateway
                .respond(RestRequest::new("GET", "/api/v1/counter"))
                .body
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
        registry
            .register_function("/counter", |_: Option<RequestBody<'_>>| Ok(1i64))
            .unwrap();
        for mount in ["", "/"] {
            let gateway = RestGateway::new(mount, Arc::clone(&registry));
            assert_eq!(
                gateway.respond(RestRequest::new("GET", "/counter")).status,
                200,
                "mount {mount:?}"
            );
        }
    }

    #[test]
    fn every_spelling_of_a_mount_names_the_same_mount() {
        // Normalized through the same function `Router::with_registry` uses, so
        // the two mount-taking APIs in this crate cannot read one string two
        // different ways.
        let registry = Arc::new(Registry::new());
        registry
            .register_function("/counter", |_: Option<RequestBody<'_>>| Ok(1i64))
            .unwrap();
        for mount in ["/api/v1", "/api/v1/", "api/v1"] {
            let gateway = RestGateway::new(mount, Arc::clone(&registry));
            assert_eq!(
                gateway
                    .respond(RestRequest::new("GET", "/api/v1/counter"))
                    .status,
                200,
                "mount {mount:?}"
            );
        }
    }

    #[test]
    fn a_doubled_slash_does_not_reach_the_root() {
        // `//` is a pointer to the empty key beneath the mount, not the mount
        // itself, so it must miss rather than dispatch to the root.
        let response = gateway().respond(RestRequest::new("GET", "/api/v1//"));
        assert_eq!(response.status, 404);
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
        let problem: ProblemBody = body_as(&response);
        assert_eq!(problem.status, 404);
        assert_eq!(problem.title, "Not Found");
        assert_eq!(problem.repe_code, Some(ErrorCode::MethodNotFound as u32));
    }

    #[test]
    fn a_problem_with_no_repe_code_omits_the_member_rather_than_nulling_it() {
        // RFC 9457 §3.2: an extension member that does not apply is absent.
        // `SkipNull` is what makes that true of the encoding rather than of the
        // struct.
        let response = gateway().respond(RestRequest::new("GET", "/api/v1x/counter"));
        assert_eq!(response.repe_error_code, None);
        let text = std::str::from_utf8(&response.body).unwrap();
        assert!(
            !text.contains("repe_code"),
            "an absent extension member must not appear at all: {text}"
        );
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
        let problem: ProblemBody = body_as(&default);
        assert!(problem.detail.contains("handler said no"));

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

    #[test]
    fn an_error_response_is_never_cacheable() {
        // RFC 9111 §4.2.2 makes 404 and 405 heuristically cacheable. A cached
        // 405 keeps its stale `Allow` after the path is registered, leaving the
        // resource unreachable through its only correct verb.
        let gateway = gateway();
        let cases = [
            gateway.respond(RestRequest::new("GET", "/api/v1/absent")),
            gateway.respond(RestRequest::new("DELETE", "/api/v1/counter")),
        ];
        for response in cases {
            assert!(response.status >= 400);
            assert_eq!(response.cache_control.as_deref(), Some("no-store"));
        }
    }

    // -- policy guards ------------------------------------------------------

    #[test]
    fn beve_request_bodies_are_refused_when_the_knob_is_off() {
        let gateway = gateway_with(RestConfig {
            accept_beve_bodies: false,
            ..RestConfig::default()
        });
        let body = structio::to_beve(&Operands { a: 1, b: 2 });
        let response =
            gateway.respond(RestRequest::new("POST", "/api/v1/add").with_body(MEDIA_BEVE, &body));
        assert_eq!(response.status, 415);

        // Responses are unaffected: encoding is driven by the server's own data,
        // so the knob is about what the gateway accepts, not what it publishes.
        let read =
            gateway.respond(RestRequest::new("GET", "/api/v1/config").with_accept(MEDIA_BEVE));
        assert_eq!(read.status, 200);
        assert_eq!(read.content_type, Some(MEDIA_BEVE));
    }

    #[test]
    fn a_deeply_nested_beve_body_is_answered_rather_than_aborting() {
        // Nesting is declared by the input, so an unbounded decoder turns a few
        // KB of anonymous request into a stack overflow — which does not unwind,
        // so no `catch` in the gateway or in hyper could contain it. It is an
        // ordinary malformed body: 400, connection intact, everything else
        // served.
        let mut body = Vec::new();
        for _ in 0..20_000 {
            body.extend_from_slice(&[0x05, 0x04]);
        }
        body.extend_from_slice(&[0x05, 0x00]);
        let response =
            gateway().respond(RestRequest::new("POST", "/api/v1/add").with_body(MEDIA_BEVE, &body));
        assert_eq!(response.status, 400);
    }

    #[test]
    fn read_only_refuses_every_mutation_and_says_so_in_allow() {
        let gateway = gateway_with(RestConfig {
            read_only: true,
            ..RestConfig::default()
        });
        for method in ["PUT", "POST"] {
            let response = gateway
                .respond(RestRequest::new(method, "/api/v1/counter").with_body(MEDIA_JSON, b"1"));
            assert_eq!(response.status, 405, "{method}");
            assert_eq!(response.allow, Some(ALLOW_READ_ONLY), "{method}");
        }
        assert_eq!(counter_of(&gateway), 7, "no refused call may have run");
    }

    #[test]
    fn options_star_respects_read_only() {
        let gateway = gateway_with(RestConfig {
            read_only: true,
            ..RestConfig::default()
        });
        let response = gateway.respond(RestRequest::new("OPTIONS", "*"));
        assert_eq!(response.status, 204);
        assert_eq!(response.allow, Some(ALLOW_READ_ONLY));
    }

    #[test]
    fn options_star_asks_about_the_server_not_a_resource() {
        // RFC 9110 §9.3.7, and the one OPTIONS form a generic HTTP tool sends.
        let response = gateway().respond(RestRequest::new("OPTIONS", "*"));
        assert_eq!(response.status, 204);
        assert_eq!(response.allow, Some(ALLOW_CALL));
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
