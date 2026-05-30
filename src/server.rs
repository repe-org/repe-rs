use crate::constants::{BodyFormat, ErrorCode};
use crate::error::RepeError;
#[cfg(not(target_arch = "wasm32"))]
use crate::io::{read_message_into, write_message_streaming};
use crate::message::{
    Message, MessageView, create_error_response_like, create_error_response_unstamped_view,
    create_response_unstamped, create_response_unstamped_view,
};
use crate::peer::{CallContext, PeerHandle};
use crate::registry::Registry;
#[cfg(not(target_arch = "wasm32"))]
use crate::server_request::route_request_view;
use crate::structs::RepeStruct;
use beve::from_slice as beve_from_slice;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::io::Write;
#[cfg(not(target_arch = "wasm32"))]
use std::io::{BufReader, BufWriter};
use std::sync::Arc;
use std::sync::Mutex;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::Ordering;
#[cfg(not(target_arch = "wasm32"))]
use std::{
    net::{TcpListener, TcpStream, ToSocketAddrs},
    sync::atomic::AtomicBool,
    thread,
    time::Duration,
};

/// Where the server should run a handler, returned by
/// [`HandlerErased::execution`]. Only the WebSocket server consults it;
/// the TCP servers always run inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive] // dispatch hint, not a wire spec; may gain modes (e.g. async off-reader)
pub enum Execution {
    /// Run on the connection's reader task, one request at a time.
    Inline,
    /// Run on a blocking thread so the reader stays free to decode
    /// further inbound frames (ACKs, cancels) while the handler runs
    /// or parks. Set by the `Router::with_*_blocking` constructors.
    OffReader,
}

/// A resolved request handler.
///
/// # Response query echo
///
/// REPE responses echo the request's query verbatim, but that echo is the
/// **dispatch layer's** responsibility, not the handler's: the built-in
/// handlers return responses with an *empty* query. The WebSocket server moves
/// the request's query buffer into the response after the handler returns; the
/// TCP ([`Server`]) and async ([`AsyncServer`](crate::AsyncServer)) servers
/// take the borrowing [`handle_view`](Self::handle_view) path and frame the
/// response with the query borrowed straight from their read buffer. Either way
/// the per-response query echo costs no allocation.
///
/// Two consequences:
///
/// * Calling [`handle`](Self::handle) / [`handle_with_ctx`](Self::handle_with_ctx)
///   directly (e.g. via [`Router::get`]) yields a response whose `query` is
///   empty — it is only filled once dispatched through a server. Code that
///   needs a complete, query-echoing response without a server should build
///   it with [`create_response`](crate::message::create_response), which
///   echoes the query itself.
/// * A *custom* implementor may set the response query itself; both the owned
///   and the borrowing dispatch paths only fill an empty one, so a hand-set
///   echo is left untouched.
pub trait HandlerErased: Send + Sync {
    fn handle(&self, req: &Message) -> Result<Message, RepeError>;

    /// Context-aware dispatch. Default implementation ignores the
    /// context and delegates to [`handle`](Self::handle), so existing
    /// implementors compile unchanged.
    ///
    /// Override this for handlers that need the calling peer (e.g.
    /// push notifies to the originator during request handling) or
    /// the dispatched method. The built-in
    /// `WebSocketServer` invokes this method per request with a
    /// [`CallContext`] carrying the peer; the TCP and async servers
    /// reach it through [`handle_view`](Self::handle_view)'s default
    /// with a peer-less [`CallContext::detached`] context, so
    /// context-aware handlers must also behave correctly when no peer
    /// is available.
    fn handle_with_ctx(&self, req: &Message, _ctx: &CallContext) -> Result<Message, RepeError> {
        self.handle(req)
    }

    /// Borrowing dispatch: handle a request given a borrowed [`MessageView`]
    /// instead of an owned [`Message`].
    ///
    /// A server that reads each frame into a reusable per-connection buffer (see
    /// [`read_message_into`]) can dispatch through this
    /// method without the per-request query and body `Vec` allocations that an
    /// owned [`Message`] requires. As with [`handle_with_ctx`](Self::handle_with_ctx),
    /// the returned response leaves its query empty; the writer supplies the
    /// echoed query from the borrowed view.
    ///
    /// The default implementation materializes an owned [`Message`] from the view
    /// (copying the query and body) and delegates to
    /// [`handle_with_ctx`](Self::handle_with_ctx), so existing handlers work
    /// unchanged. The built-in JSON and typed handlers ([`Router::with_json`] /
    /// [`Router::with_typed`]) override it to decode straight from the borrowed
    /// body and skip those copies; the other built-ins (context-aware, struct,
    /// registry, and any middleware-wrapped route) use the owning default until
    /// they are likewise overridden.
    fn handle_view(&self, view: &MessageView, ctx: &CallContext) -> Result<Message, RepeError> {
        self.handle_with_ctx(&view.to_message(), ctx)
    }

    /// Where this handler should be dispatched. Defaults to
    /// [`Execution::Inline`]. The `Router::with_*_blocking` constructors
    /// return a wrapper whose `execution` is [`Execution::OffReader`];
    /// only the WebSocket server checks it.
    fn execution(&self) -> Execution {
        Execution::Inline
    }
}

pub struct Next<'a> {
    middlewares: &'a [Arc<dyn Middleware>],
    handler: &'a dyn HandlerErased,
    ctx: Option<&'a CallContext<'a>>,
}

impl<'a> Next<'a> {
    fn new(middlewares: &'a [Arc<dyn Middleware>], handler: &'a dyn HandlerErased) -> Self {
        Self {
            middlewares,
            handler,
            ctx: None,
        }
    }

    fn with_ctx(
        middlewares: &'a [Arc<dyn Middleware>],
        handler: &'a dyn HandlerErased,
        ctx: &'a CallContext<'a>,
    ) -> Self {
        Self {
            middlewares,
            handler,
            ctx: Some(ctx),
        }
    }

    /// Continue to the next middleware (or final handler if this was the last middleware).
    ///
    /// If a [`CallContext`] was attached upstream (via
    /// `WebSocketServer`'s peer-aware dispatch path), it is threaded
    /// through to the leaf handler automatically. Middleware authors
    /// do not need to be aware of the context to forward it; calling
    /// `next.run(req)` preserves whatever the caller provided.
    pub fn run(self, req: &Message) -> Result<Message, RepeError> {
        if let Some((first, rest)) = self.middlewares.split_first() {
            first.handle(
                req,
                Next {
                    middlewares: rest,
                    handler: self.handler,
                    ctx: self.ctx,
                },
            )
        } else {
            match self.ctx {
                Some(ctx) => self.handler.handle_with_ctx(req, ctx),
                None => self.handler.handle(req),
            }
        }
    }

    /// The [`CallContext`] threaded into this pipeline, if any.
    ///
    /// `WebSocketServer`'s peer-aware dispatch attaches a context
    /// carrying the calling peer; the TCP transports and direct
    /// in-process dispatch do not, so this returns `None` there.
    /// Lets a cross-cutting middleware read the calling peer (via
    /// [`ctx.peer()`](CallContext::peer)) or the dispatched method
    /// without being a context-aware leaf handler itself.
    pub fn ctx(&self) -> Option<&CallContext<'a>> {
        self.ctx
    }

    /// The calling peer, if one was attached upstream. Sugar for
    /// [`ctx()`](Self::ctx)`.and_then(|c| c.peer())`. Returns `None`
    /// for dispatch paths without a peer (TCP transports, direct
    /// calls).
    pub fn peer(&self) -> Option<&PeerHandle> {
        self.ctx.and_then(|c| c.peer())
    }
}

pub trait Middleware: Send + Sync {
    fn handle(&self, req: &Message, next: Next<'_>) -> Result<Message, RepeError>;
}

impl<F> Middleware for F
where
    F: for<'a> Fn(&'a Message, Next<'a>) -> Result<Message, RepeError> + Send + Sync + 'static,
{
    fn handle(&self, req: &Message, next: Next<'_>) -> Result<Message, RepeError> {
        (self)(req, next)
    }
}

struct MiddlewarePipeline {
    handler: Arc<dyn HandlerErased>,
    middlewares: Arc<Vec<Arc<dyn Middleware>>>,
}

impl HandlerErased for MiddlewarePipeline {
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        Next::new(self.middlewares.as_ref(), self.handler.as_ref()).run(req)
    }

    fn handle_with_ctx(&self, req: &Message, ctx: &CallContext) -> Result<Message, RepeError> {
        Next::with_ctx(self.middlewares.as_ref(), self.handler.as_ref(), ctx).run(req)
    }

    fn execution(&self) -> Execution {
        // Forward to the wrapped handler so a middleware-wrapped
        // off-reader path stays off-reader. Without this, registering
        // any middleware would silently downgrade every `_blocking`
        // handler to inline and reintroduce the streaming deadlock.
        self.handler.execution()
    }
}

/// Wraps any handler so it dispatches off the reader task
/// ([`Execution::OffReader`]); delegates `handle` / `handle_with_ctx`
/// unchanged. Built by the `Router::with_*_blocking` constructors.
struct OffReaderHandler<H>(H);

impl<H: HandlerErased> HandlerErased for OffReaderHandler<H> {
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        self.0.handle(req)
    }

    fn handle_with_ctx(&self, req: &Message, ctx: &CallContext) -> Result<Message, RepeError> {
        self.0.handle_with_ctx(req, ctx)
    }

    fn execution(&self) -> Execution {
        Execution::OffReader
    }
}

fn decode_json_param(req: &Message) -> Result<Result<Value, Message>, RepeError> {
    let value: Value = match BodyFormat::try_from(req.header.body_format) {
        Ok(BodyFormat::Json) => serde_json::from_slice(&req.body)?,
        Ok(BodyFormat::Utf8) => serde_json::from_str(&req.body_utf8()).map_err(RepeError::from)?,
        Ok(BodyFormat::Beve) => beve_from_slice(&req.body)?,
        _ => {
            return Ok(Err(create_error_response_like(
                req,
                ErrorCode::InvalidBody,
                "Expected JSON body",
            )));
        }
    };
    Ok(Ok(value))
}

/// Borrowing twin of [`decode_json_param`]: decode the request body straight
/// from the borrowed view, with no owned `Message`. The UTF-8 branch parses the
/// bytes directly (`from_slice`) rather than via an intermediate `String`.
fn decode_json_param_view(view: &MessageView) -> Result<Result<Value, Message>, RepeError> {
    let value: Value = match BodyFormat::try_from(view.header.body_format) {
        Ok(BodyFormat::Json) | Ok(BodyFormat::Utf8) => serde_json::from_slice(view.body)?,
        Ok(BodyFormat::Beve) => beve_from_slice(view.body)?,
        _ => {
            return Ok(Err(create_error_response_unstamped_view(
                view,
                ErrorCode::InvalidBody,
                "Expected JSON body",
            )));
        }
    };
    Ok(Ok(value))
}

struct JsonHandler<F>(F)
where
    F: Fn(Value) -> Result<Value, (ErrorCode, String)> + Send + Sync + 'static;

impl<F> HandlerErased for JsonHandler<F>
where
    F: Fn(Value) -> Result<Value, (ErrorCode, String)> + Send + Sync + 'static,
{
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        let param = match decode_json_param(req)? {
            Ok(v) => v,
            Err(err) => return Ok(err),
        };
        match (self.0)(param) {
            Ok(value) => create_response_unstamped(req, value, BodyFormat::Json),
            Err((code, msg)) => Ok(create_error_response_like(req, code, msg)),
        }
    }

    fn handle_view(&self, view: &MessageView, _ctx: &CallContext) -> Result<Message, RepeError> {
        let param = match decode_json_param_view(view)? {
            Ok(v) => v,
            Err(err) => return Ok(err),
        };
        match (self.0)(param) {
            Ok(value) => create_response_unstamped_view(view, value, BodyFormat::Json),
            Err((code, msg)) => Ok(create_error_response_unstamped_view(view, code, msg)),
        }
    }
}

struct JsonHandlerCtx<F>(F)
where
    F: Fn(&CallContext, Value) -> Result<Value, (ErrorCode, String)> + Send + Sync + 'static;

impl<F> HandlerErased for JsonHandlerCtx<F>
where
    F: Fn(&CallContext, Value) -> Result<Value, (ErrorCode, String)> + Send + Sync + 'static,
{
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        let path = req.query_str().unwrap_or("");
        let ctx = CallContext::detached(path);
        self.handle_with_ctx(req, &ctx)
    }

    fn handle_with_ctx(&self, req: &Message, ctx: &CallContext) -> Result<Message, RepeError> {
        let param = match decode_json_param(req)? {
            Ok(v) => v,
            Err(err) => return Ok(err),
        };
        match (self.0)(ctx, param) {
            Ok(value) => create_response_unstamped(req, value, BodyFormat::Json),
            Err((code, msg)) => Ok(create_error_response_like(req, code, msg)),
        }
    }
}

/// Wrapper that lets typed handlers override the response [`BodyFormat`].
/// Use helpers like [`TypedResponse::json`] or [`TypedResponse::beve`] to keep call sites concise.
pub struct TypedResponse<R> {
    value: R,
    format: BodyFormat,
}

impl<R> TypedResponse<R> {
    pub fn new(value: R, format: BodyFormat) -> Self {
        Self { value, format }
    }

    pub fn json(value: R) -> Self {
        Self::new(value, BodyFormat::Json)
    }

    pub fn beve(value: R) -> Self {
        Self::new(value, BodyFormat::Beve)
    }

    pub fn utf8(value: R) -> Self {
        Self::new(value, BodyFormat::Utf8)
    }

    pub fn raw_binary(value: R) -> Self {
        Self::new(value, BodyFormat::RawBinary)
    }
}

pub trait IntoTypedResponse<R> {
    fn into_typed_response(self) -> (R, BodyFormat);
}

impl<R> IntoTypedResponse<R> for R {
    fn into_typed_response(self) -> (R, BodyFormat) {
        (self, BodyFormat::Json)
    }
}

impl<R> IntoTypedResponse<R> for TypedResponse<R> {
    fn into_typed_response(self) -> (R, BodyFormat) {
        (self.value, self.format)
    }
}

pub trait TypedHandlerFn<T, R>: Send + Sync + 'static
where
    T: DeserializeOwned + Send + Sync + 'static,
    R: Serialize + Send + Sync + 'static,
{
    type Response: IntoTypedResponse<R>;
    fn call(&self, input: T) -> Result<Self::Response, (ErrorCode, String)>;
}

impl<T, R, S, F> TypedHandlerFn<T, R> for F
where
    T: DeserializeOwned + Send + Sync + 'static,
    R: Serialize + Send + Sync + 'static,
    S: IntoTypedResponse<R>,
    F: Fn(T) -> Result<S, (ErrorCode, String)> + Send + Sync + 'static,
{
    type Response = S;

    fn call(&self, input: T) -> Result<Self::Response, (ErrorCode, String)> {
        (self)(input)
    }
}

fn decode_typed_param<T>(req: &Message) -> Result<Result<T, Message>, RepeError>
where
    T: DeserializeOwned,
{
    let value: T = match BodyFormat::try_from(req.header.body_format) {
        Ok(BodyFormat::Json) => serde_json::from_slice(&req.body)?,
        Ok(BodyFormat::Utf8) => serde_json::from_str(&req.body_utf8()).map_err(RepeError::from)?,
        Ok(BodyFormat::Beve) => beve_from_slice(&req.body)?,
        _ => {
            return Ok(Err(create_error_response_like(
                req,
                ErrorCode::InvalidBody,
                "Expected JSON body",
            )));
        }
    };
    Ok(Ok(value))
}

/// Borrowing twin of [`decode_typed_param`]: deserialize `T` straight from the
/// borrowed view body, with no owned `Message` and no intermediate `String` on
/// the UTF-8 branch.
fn decode_typed_param_view<T>(view: &MessageView) -> Result<Result<T, Message>, RepeError>
where
    T: DeserializeOwned,
{
    let value: T = match BodyFormat::try_from(view.header.body_format) {
        Ok(BodyFormat::Json) | Ok(BodyFormat::Utf8) => serde_json::from_slice(view.body)?,
        Ok(BodyFormat::Beve) => beve_from_slice(view.body)?,
        _ => {
            return Ok(Err(create_error_response_unstamped_view(
                view,
                ErrorCode::InvalidBody,
                "Expected JSON body",
            )));
        }
    };
    Ok(Ok(value))
}

struct TypedHandler<T, R, F>(F, std::marker::PhantomData<(T, R)>)
where
    T: DeserializeOwned + Send + Sync + 'static,
    R: Serialize + Send + Sync + 'static,
    F: TypedHandlerFn<T, R>;

impl<T, R, F> HandlerErased for TypedHandler<T, R, F>
where
    T: DeserializeOwned + Send + Sync + 'static,
    R: Serialize + Send + Sync + 'static,
    F: TypedHandlerFn<T, R>,
{
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        let t: T = match decode_typed_param(req)? {
            Ok(v) => v,
            Err(err) => return Ok(err),
        };
        match self.0.call(t) {
            Ok(r) => {
                let (value, format) = r.into_typed_response();
                create_response_unstamped(req, value, format)
            }
            Err((code, msg)) => Ok(create_error_response_like(req, code, msg)),
        }
    }

    fn handle_view(&self, view: &MessageView, _ctx: &CallContext) -> Result<Message, RepeError> {
        let t: T = match decode_typed_param_view(view)? {
            Ok(v) => v,
            Err(err) => return Ok(err),
        };
        match self.0.call(t) {
            Ok(r) => {
                let (value, format) = r.into_typed_response();
                create_response_unstamped_view(view, value, format)
            }
            Err((code, msg)) => Ok(create_error_response_unstamped_view(view, code, msg)),
        }
    }
}

/// Trait shape mirroring [`TypedHandlerFn`] for context-aware typed
/// handlers. Implemented by closures of the form
/// `Fn(&CallContext, T) -> Result<R, (ErrorCode, String)>`.
pub trait TypedHandlerFnCtx<T, R>: Send + Sync + 'static
where
    T: DeserializeOwned + Send + Sync + 'static,
    R: Serialize + Send + Sync + 'static,
{
    type Response: IntoTypedResponse<R>;
    fn call(&self, ctx: &CallContext, input: T) -> Result<Self::Response, (ErrorCode, String)>;
}

impl<T, R, S, F> TypedHandlerFnCtx<T, R> for F
where
    T: DeserializeOwned + Send + Sync + 'static,
    R: Serialize + Send + Sync + 'static,
    S: IntoTypedResponse<R>,
    F: Fn(&CallContext, T) -> Result<S, (ErrorCode, String)> + Send + Sync + 'static,
{
    type Response = S;

    fn call(&self, ctx: &CallContext, input: T) -> Result<Self::Response, (ErrorCode, String)> {
        (self)(ctx, input)
    }
}

struct TypedHandlerCtx<T, R, F>(F, std::marker::PhantomData<(T, R)>)
where
    T: DeserializeOwned + Send + Sync + 'static,
    R: Serialize + Send + Sync + 'static,
    F: TypedHandlerFnCtx<T, R>;

impl<T, R, F> HandlerErased for TypedHandlerCtx<T, R, F>
where
    T: DeserializeOwned + Send + Sync + 'static,
    R: Serialize + Send + Sync + 'static,
    F: TypedHandlerFnCtx<T, R>,
{
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        let path = req.query_str().unwrap_or("");
        let ctx = CallContext::detached(path);
        self.handle_with_ctx(req, &ctx)
    }

    fn handle_with_ctx(&self, req: &Message, ctx: &CallContext) -> Result<Message, RepeError> {
        let t: T = match decode_typed_param(req)? {
            Ok(v) => v,
            Err(err) => return Ok(err),
        };
        match self.0.call(ctx, t) {
            Ok(r) => {
                let (value, format) = r.into_typed_response();
                create_response_unstamped(req, value, format)
            }
            Err((code, msg)) => Ok(create_error_response_like(req, code, msg)),
        }
    }
}

/// One handler entry in `Router::inner`.
///
/// `raw` is the bare handler as registered; `dispatched` is `raw` already
/// wrapped in any active [`MiddlewarePipeline`]. Keeping both means dispatch
/// is a single `Arc::clone(&entry.dispatched)` (no per-request wrap allocation)
/// while [`Router::register_middleware`] can still rebuild the wrapped form
/// across every existing entry when the middleware chain changes.
///
/// With no middleware registered, `raw` and `dispatched` are clones of the
/// same `Arc` — no extra allocation for plain routes.
#[derive(Clone)]
struct RouterMapEntry {
    raw: Arc<dyn HandlerErased>,
    dispatched: Arc<dyn HandlerErased>,
}

/// Internal router entry for a [`Registry`] mounted at a fixed prefix.
///
/// Holds the normalized prefix once for path matching plus both the raw
/// `Arc<RegisteredRegistry>` (coerced to `Arc<dyn HandlerErased>`) and the
/// pre-middleware-wrapped form used at dispatch. Lookup is a prefix check plus
/// an `Arc` refcount bump; the per-request `String` + `Arc::new` that the
/// original `RegistryRequestHandler` cost — and the per-request
/// `MiddlewarePipeline` allocation — have been folded away.
#[derive(Clone)]
struct RegistryEntry {
    prefix: String,
    raw: Arc<dyn HandlerErased>,
    dispatched: Arc<dyn HandlerErased>,
}

impl RegistryEntry {
    fn matches(&self, path: &str) -> bool {
        if self.prefix.is_empty() {
            return true;
        }
        if path == self.prefix {
            return true;
        }
        path.strip_prefix(&self.prefix)
            .is_some_and(|rest| rest.starts_with('/'))
    }
}

/// Internal router entry for a [`RepeStruct`] mounted at a fixed root.
///
/// Holds the normalized root once for path matching plus both the raw
/// `Arc<RegisteredStruct<T, L>>` (coerced to `Arc<dyn HandlerErased>`) and the
/// pre-middleware-wrapped form used at dispatch. Lookup is a prefix check plus
/// an `Arc` refcount bump; the per-request `Vec<String>` from
/// `json_pointer::parse`, the per-request `String`, the per-request
/// `Arc::new` that the original `StructRequestHandler` cost, and the
/// per-request `MiddlewarePipeline` allocation have all been folded away.
#[derive(Clone)]
struct StructEntry {
    root: String,
    raw: Arc<dyn HandlerErased>,
    dispatched: Arc<dyn HandlerErased>,
}

impl StructEntry {
    fn matches(&self, path: &str) -> bool {
        if self.root.is_empty() {
            return true;
        }
        if path == self.root {
            return true;
        }
        path.strip_prefix(&self.root)
            .is_some_and(|rest| rest.starts_with('/'))
    }
}

/// Wrap `handler` in the active middleware pipeline, or return it unchanged
/// when no middleware is registered. Built once at registration / middleware
/// change rather than per dispatch.
fn wrap_with_middlewares(
    handler: &Arc<dyn HandlerErased>,
    middlewares: &Arc<Vec<Arc<dyn Middleware>>>,
) -> Arc<dyn HandlerErased> {
    if middlewares.is_empty() {
        Arc::clone(handler)
    } else {
        Arc::new(MiddlewarePipeline {
            handler: Arc::clone(handler),
            middlewares: Arc::clone(middlewares),
        })
    }
}

#[derive(Debug)]
pub enum LockError {
    Poisoned(String),
    Other(String),
}

impl LockError {
    pub fn poisoned(err: impl std::fmt::Display) -> Self {
        Self::Poisoned(err.to_string())
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Poisoned(msg) | LockError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for LockError {}

pub trait Lockable<T: ?Sized>: Send + Sync {
    type Guard<'a>: std::ops::DerefMut<Target = T> + 'a
    where
        Self: 'a;

    fn lock(&self) -> Result<Self::Guard<'_>, LockError>;
}

impl<T: ?Sized + Send> Lockable<T> for Mutex<T> {
    type Guard<'a>
        = std::sync::MutexGuard<'a, T>
    where
        Self: 'a;

    fn lock(&self) -> Result<Self::Guard<'_>, LockError> {
        std::sync::Mutex::lock(self).map_err(LockError::from)
    }
}

impl<G> From<std::sync::PoisonError<G>> for LockError {
    fn from(err: std::sync::PoisonError<G>) -> Self {
        LockError::Poisoned(err.to_string())
    }
}

impl<T: ?Sized + Send + Sync> Lockable<T> for std::sync::RwLock<T> {
    type Guard<'a>
        = std::sync::RwLockWriteGuard<'a, T>
    where
        Self: 'a;

    fn lock(&self) -> Result<Self::Guard<'_>, LockError> {
        std::sync::RwLock::write(self).map_err(LockError::from)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl<T: ?Sized + Send> Lockable<T> for tokio::sync::Mutex<T> {
    type Guard<'a>
        = tokio::sync::MutexGuard<'a, T>
    where
        Self: 'a;

    fn lock(&self) -> Result<Self::Guard<'_>, LockError> {
        Ok(self.blocking_lock())
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl<T: ?Sized + Send + Sync> Lockable<T> for tokio::sync::RwLock<T> {
    type Guard<'a>
        = tokio::sync::RwLockWriteGuard<'a, T>
    where
        Self: 'a;

    fn lock(&self) -> Result<Self::Guard<'_>, LockError> {
        Ok(self.blocking_write())
    }
}

#[cfg(feature = "parking-lot")]
impl<T: ?Sized + Send> Lockable<T> for parking_lot::Mutex<T> {
    type Guard<'a>
        = parking_lot::MutexGuard<'a, T>
    where
        Self: 'a;

    fn lock(&self) -> Result<Self::Guard<'_>, LockError> {
        Ok(self.lock())
    }
}

#[cfg(feature = "parking-lot")]
impl<T: ?Sized + Send + Sync> Lockable<T> for parking_lot::RwLock<T> {
    type Guard<'a>
        = parking_lot::RwLockWriteGuard<'a, T>
    where
        Self: 'a;

    fn lock(&self) -> Result<Self::Guard<'_>, LockError> {
        Ok(self.write())
    }
}

#[derive(Clone)]
pub struct Router {
    inner: Arc<HashMap<String, RouterMapEntry>>,
    structs: Arc<Vec<StructEntry>>,
    registries: Arc<Vec<RegistryEntry>>,
    middlewares: Arc<Vec<Arc<dyn Middleware>>>,
}

impl Router {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(HashMap::new()),
            structs: Arc::new(Vec::new()),
            registries: Arc::new(Vec::new()),
            middlewares: Arc::new(Vec::new()),
        }
    }

    /// Insert a raw handler into `self.inner`, pre-wrapping it in the active
    /// middleware pipeline. Centralizes the boilerplate every `with_*`
    /// registrar would otherwise repeat, and keeps the wrap-on-registration
    /// invariant in one place so [`register_middleware`] only has to rebuild
    /// the `dispatched` slot.
    fn insert_route(&mut self, path: &str, raw: Arc<dyn HandlerErased>) {
        let dispatched = wrap_with_middlewares(&raw, &self.middlewares);
        let mut map = self.inner.as_ref().clone();
        map.insert(path.to_string(), RouterMapEntry { raw, dispatched });
        self.inner = Arc::new(map);
    }

    /// Add a JSON Value-based handler. Alias: `with`.
    pub fn with_json(
        mut self,
        path: &str,
        handler: impl Fn(Value) -> Result<Value, (ErrorCode, String)> + Send + Sync + 'static,
    ) -> Self {
        self.insert_route(path, Arc::new(JsonHandler(handler)));
        self
    }

    /// Attach a middleware that runs before handlers and can short-circuit requests.
    pub fn with_middleware(mut self, middleware: impl Middleware + 'static) -> Self {
        self.register_middleware(middleware);
        self
    }

    /// Register middleware in-place so callers can retain the shared handle.
    ///
    /// Existing routes have their `dispatched` slot rebuilt against the new
    /// middleware chain so future lookups remain a single `Arc::clone` with
    /// no per-request wrap allocation. The rebuild is a one-time `Arc::new`
    /// per route, paid only here.
    ///
    /// Cost note: each call walks every registered route once, so chaining
    /// `N` middleware registrations after `K` routes is `O(N·K)` setup. This
    /// only runs at builder time, never on the dispatch hot path, and the
    /// typical usage shape (a handful of middlewares registered once) is
    /// nowhere near that worst case. A future batch helper that takes
    /// several middlewares at once could amortize the rebuild to `O(K)`.
    pub fn register_middleware(
        &mut self,
        middleware: impl Middleware + 'static,
    ) -> Arc<dyn Middleware> {
        let mut list = (*self.middlewares).clone();
        let arc: Arc<dyn Middleware> = Arc::new(middleware);
        list.push(arc.clone());
        self.middlewares = Arc::new(list);

        // Rebuild dispatched slots so middleware applies uniformly to every
        // already-registered route (HashMap entries, registries, structs).
        let rebuilt_map: HashMap<String, RouterMapEntry> = self
            .inner
            .iter()
            .map(|(path, entry)| {
                let dispatched = wrap_with_middlewares(&entry.raw, &self.middlewares);
                (
                    path.clone(),
                    RouterMapEntry {
                        raw: Arc::clone(&entry.raw),
                        dispatched,
                    },
                )
            })
            .collect();
        self.inner = Arc::new(rebuilt_map);

        let rebuilt_registries: Vec<RegistryEntry> = self
            .registries
            .iter()
            .map(|entry| RegistryEntry {
                prefix: entry.prefix.clone(),
                raw: Arc::clone(&entry.raw),
                dispatched: wrap_with_middlewares(&entry.raw, &self.middlewares),
            })
            .collect();
        self.registries = Arc::new(rebuilt_registries);

        let rebuilt_structs: Vec<StructEntry> = self
            .structs
            .iter()
            .map(|entry| StructEntry {
                root: entry.root.clone(),
                raw: Arc::clone(&entry.raw),
                dispatched: wrap_with_middlewares(&entry.raw, &self.middlewares),
            })
            .collect();
        self.structs = Arc::new(rebuilt_structs);

        arc
    }

    /// Backwards-compatible alias for `with_json`.
    pub fn with(
        self,
        path: &str,
        handler: impl Fn(Value) -> Result<Value, (ErrorCode, String)> + Send + Sync + 'static,
    ) -> Self {
        self.with_json(path, handler)
    }

    /// Add a typed handler that auto-deserializes JSON/UTF-8/BEVE into `T` and serializes `R`.
    /// Return `Ok(value)` for JSON responses or wrap results with [`TypedResponse`] to pick a
    /// different [`BodyFormat`].
    pub fn with_typed<T, R, F>(mut self, path: &str, f: F) -> Self
    where
        T: DeserializeOwned + Send + Sync + 'static,
        R: Serialize + Send + Sync + 'static,
        F: TypedHandlerFn<T, R>,
    {
        self.insert_route(
            path,
            Arc::new(TypedHandler::<T, R, F>(f, std::marker::PhantomData)),
        );
        self
    }

    /// Context-aware JSON handler. Same shape as [`with_json`] but the
    /// closure also receives a [`CallContext`] carrying the calling
    /// peer (when one is available) and the dispatched method.
    ///
    /// Typical use: push a notify back to the calling peer during
    /// request handling, e.g. progress updates while a long-running
    /// call is in flight.
    ///
    /// ```ignore
    /// use repe::server::Router;
    /// use repe::NotifyBody;
    ///
    /// let router = Router::new().with_json_ctx("/start", |ctx, params| {
    ///     if let Some(peer) = ctx.peer() {
    ///         let _ = peer.send_notify(
    ///             "/progress",
    ///             NotifyBody::Json(serde_json::to_vec(&serde_json::json!({
    ///                 "stage": "begin"
    ///             })).unwrap()),
    ///         );
    ///     }
    ///     Ok(serde_json::json!({ "started": true }))
    /// });
    /// ```
    ///
    /// When dispatched without a peer (TCP transports, direct
    /// in-process calls), `ctx.peer()` returns `None`.
    ///
    /// [`with_json`]: Self::with_json
    pub fn with_json_ctx<F>(mut self, path: &str, handler: F) -> Self
    where
        F: Fn(&CallContext, Value) -> Result<Value, (ErrorCode, String)> + Send + Sync + 'static,
    {
        self.insert_route(path, Arc::new(JsonHandlerCtx(handler)));
        self
    }

    /// Context-aware typed handler. Same shape as [`with_typed`] but
    /// the closure also receives a [`CallContext`].
    ///
    /// [`with_typed`]: Self::with_typed
    pub fn with_typed_ctx<T, R, F>(mut self, path: &str, f: F) -> Self
    where
        T: DeserializeOwned + Send + Sync + 'static,
        R: Serialize + Send + Sync + 'static,
        F: TypedHandlerFnCtx<T, R>,
    {
        self.insert_route(
            path,
            Arc::new(TypedHandlerCtx::<T, R, F>(f, std::marker::PhantomData)),
        );
        self
    }

    /// Off-reader variant of [`with_json`](Self::with_json): on the
    /// WebSocket server the handler runs on a blocking thread (see
    /// [`Execution::OffReader`]) so the reader stays free to decode
    /// further inbound frames while it runs or parks. Use for handlers
    /// that block — e.g. a `repe::stream` producer waiting on
    /// `wait_for_credit`. On the TCP servers it behaves exactly like
    /// [`with_json`](Self::with_json).
    pub fn with_json_blocking(
        mut self,
        path: &str,
        handler: impl Fn(Value) -> Result<Value, (ErrorCode, String)> + Send + Sync + 'static,
    ) -> Self {
        self.insert_route(path, Arc::new(OffReaderHandler(JsonHandler(handler))));
        self
    }

    /// Off-reader variant of [`with_json_ctx`](Self::with_json_ctx).
    /// See [`with_json_blocking`](Self::with_json_blocking).
    pub fn with_json_ctx_blocking<F>(mut self, path: &str, handler: F) -> Self
    where
        F: Fn(&CallContext, Value) -> Result<Value, (ErrorCode, String)> + Send + Sync + 'static,
    {
        self.insert_route(path, Arc::new(OffReaderHandler(JsonHandlerCtx(handler))));
        self
    }

    /// Off-reader variant of [`with_typed`](Self::with_typed).
    /// See [`with_json_blocking`](Self::with_json_blocking).
    pub fn with_typed_blocking<T, R, F>(mut self, path: &str, f: F) -> Self
    where
        T: DeserializeOwned + Send + Sync + 'static,
        R: Serialize + Send + Sync + 'static,
        F: TypedHandlerFn<T, R>,
    {
        self.insert_route(
            path,
            Arc::new(OffReaderHandler(TypedHandler::<T, R, F>(
                f,
                std::marker::PhantomData,
            ))),
        );
        self
    }

    /// Off-reader variant of [`with_typed_ctx`](Self::with_typed_ctx).
    /// See [`with_json_blocking`](Self::with_json_blocking).
    pub fn with_typed_ctx_blocking<T, R, F>(mut self, path: &str, f: F) -> Self
    where
        T: DeserializeOwned + Send + Sync + 'static,
        R: Serialize + Send + Sync + 'static,
        F: TypedHandlerFnCtx<T, R>,
    {
        self.insert_route(
            path,
            Arc::new(OffReaderHandler(TypedHandlerCtx::<T, R, F>(
                f,
                std::marker::PhantomData,
            ))),
        );
        self
    }

    /// A pluggable trait for typed JSON handlers.
    /// Implement this trait on your service type to expose a method as a route.
    pub fn with_handler<H>(mut self, path: &str, handler: H) -> Self
    where
        H: JsonTypedHandler + 'static,
    {
        self.insert_route(path, Arc::new(JsonTypedAdapter(handler)));
        self
    }

    /// Register a struct that implements [`RepeStruct`].
    ///
    /// The struct is wrapped in an [`Arc`] of any lock implementing [`Lockable`] so that the
    /// caller can retain shared ownership and mutate state while the router serves requests.
    /// This includes `std::sync::Mutex`/`RwLock`, `tokio::sync::Mutex`/`RwLock`, and (with the
    /// `parking-lot` feature) `parking_lot` synchronization primitives.
    pub fn with_struct_shared<T, L>(mut self, root: &str, shared: Arc<L>) -> Self
    where
        T: RepeStruct + 'static,
        L: Lockable<T> + 'static,
    {
        self.register_struct_shared::<T, L>(root, shared);
        self
    }

    /// Register a struct in-place and get back the shared handle without breaking builder chains.
    pub fn register_struct_shared<T, L>(&mut self, root: &str, shared: Arc<L>) -> Arc<L>
    where
        T: RepeStruct + 'static,
        L: Lockable<T> + 'static,
    {
        let registered = Arc::new(RegisteredStruct::<T, L>::new(root, Arc::clone(&shared)));
        let root = registered.root.clone();
        // `Arc<RegisteredStruct<T, L>>` coerces to `Arc<dyn HandlerErased>`
        // because `RegisteredStruct: HandlerErased + Sized`. The handler is
        // built once at registration so dispatch is a clone of this Arc, not
        // a fresh allocation.
        let raw: Arc<dyn HandlerErased> = registered;
        let dispatched = wrap_with_middlewares(&raw, &self.middlewares);
        let entry = StructEntry {
            root,
            raw,
            dispatched,
        };
        let mut entries = (*self.structs).clone();
        entries.push(entry);
        self.structs = Arc::new(entries);
        shared
    }

    /// Convenience helper to register an owned struct value. Returns the shared handle so callers
    /// can keep interacting with the registered object.
    pub fn with_struct<T>(mut self, root: &str, value: T) -> (Self, Arc<Mutex<T>>)
    where
        T: RepeStruct + 'static,
    {
        let shared = self.register_struct(root, value);
        (self, shared)
    }

    /// Builder-friendly helper to register owned structs while keeping the router.
    pub fn register_struct<T>(&mut self, root: &str, value: T) -> Arc<Mutex<T>>
    where
        T: RepeStruct + 'static,
    {
        let shared = Arc::new(Mutex::new(value));
        self.register_struct_shared::<T, Mutex<T>>(root, Arc::clone(&shared));
        shared
    }

    /// Register a dynamic [`Registry`] under `path_prefix`.
    ///
    /// The prefix can be empty to serve the registry at the root path. Requests under the
    /// prefix are mapped to registry JSON pointers by stripping the prefix.
    pub fn with_registry(mut self, path_prefix: &str, registry: Arc<Registry>) -> Self {
        self.register_registry(path_prefix, registry);
        self
    }

    /// Register a dynamic [`Registry`] in-place and return the shared handle.
    pub fn register_registry(
        &mut self,
        path_prefix: &str,
        registry: Arc<Registry>,
    ) -> Arc<Registry> {
        let registered = Arc::new(RegisteredRegistry::new(path_prefix, Arc::clone(&registry)));
        let prefix = registered.prefix.clone();
        // `Arc<RegisteredRegistry>` coerces to `Arc<dyn HandlerErased>` — the
        // handler is built once at registration so dispatch is a clone of this
        // Arc, not a fresh allocation.
        let raw: Arc<dyn HandlerErased> = registered;
        let dispatched = wrap_with_middlewares(&raw, &self.middlewares);
        let entry = RegistryEntry {
            prefix,
            raw,
            dispatched,
        };
        let mut entries = (*self.registries).clone();
        entries.push(entry);
        self.registries = Arc::new(entries);
        registry
    }

    pub fn get(&self, path: &str) -> Option<Arc<dyn HandlerErased>> {
        if let Some(entry) = self.inner.get(path) {
            return Some(Arc::clone(&entry.dispatched));
        }
        if let Some(entry) = self.registries.iter().find(|entry| entry.matches(path)) {
            return Some(Arc::clone(&entry.dispatched));
        }
        self.structs
            .iter()
            .find(|entry| entry.matches(path))
            .map(|entry| Arc::clone(&entry.dispatched))
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

struct RegisteredStruct<T, L>
where
    T: RepeStruct + 'static,
    L: Lockable<T> + 'static,
{
    root: String,
    shared: Arc<L>,
    phantom: std::marker::PhantomData<T>,
}

struct RegisteredRegistry {
    prefix: String,
    registry: Arc<Registry>,
}

impl RegisteredRegistry {
    fn new(prefix: &str, registry: Arc<Registry>) -> Self {
        let mut normalized = if prefix.is_empty() || prefix == "/" {
            String::new()
        } else if prefix.starts_with('/') {
            prefix.to_string()
        } else {
            format!("/{}", prefix)
        };
        if normalized.len() > 1 {
            normalized = normalized.trim_end_matches('/').to_string();
        }
        Self {
            prefix: normalized,
            registry,
        }
    }

    fn pointer_for<'a>(&self, path: &'a str) -> Option<&'a str> {
        if self.prefix.is_empty() {
            return if path.is_empty() {
                Some("/")
            } else {
                Some(path)
            };
        }

        if path == self.prefix {
            return Some("/");
        }

        let rest = path.strip_prefix(&self.prefix)?;
        if rest.starts_with('/') {
            Some(rest)
        } else {
            None
        }
    }
}

impl HandlerErased for RegisteredRegistry {
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        let path = req.query_str().unwrap_or("");
        let Some(pointer) = self.pointer_for(path) else {
            return Ok(create_error_response_like(
                req,
                ErrorCode::MethodNotFound,
                format!("path is not below registry prefix: {path}"),
            ));
        };
        let body = match Registry::decode_body(req) {
            Ok(value) => value,
            Err(err) => return Ok(create_error_response_like(req, err.code(), err.to_string())),
        };
        match self.registry.dispatch(pointer, body) {
            Ok(value) => create_response_unstamped(req, value, BodyFormat::Json),
            Err(err) => Ok(create_error_response_like(req, err.code(), err.to_string())),
        }
    }

    fn handle_with_ctx(&self, req: &Message, ctx: &CallContext) -> Result<Message, RepeError> {
        let path = req.query_str().unwrap_or("");
        let Some(pointer) = self.pointer_for(path) else {
            return Ok(create_error_response_like(
                req,
                ErrorCode::MethodNotFound,
                format!("path is not below registry prefix: {path}"),
            ));
        };
        let body = match Registry::decode_body(req) {
            Ok(value) => value,
            Err(err) => return Ok(create_error_response_like(req, err.code(), err.to_string())),
        };
        match self.registry.dispatch_with_ctx(pointer, body, ctx) {
            Ok(value) => create_response_unstamped(req, value, BodyFormat::Json),
            Err(err) => Ok(create_error_response_like(req, err.code(), err.to_string())),
        }
    }
}

impl<T, L> RegisteredStruct<T, L>
where
    T: RepeStruct + 'static,
    L: Lockable<T> + 'static,
{
    fn new(root: &str, shared: Arc<L>) -> Self {
        let normalized = if root.is_empty() || root == "/" {
            String::new()
        } else if root.starts_with('/') {
            root.to_string()
        } else {
            format!("/{}", root)
        };
        Self {
            root: normalized,
            shared,
            phantom: std::marker::PhantomData,
        }
    }

    fn relative_pointer<'a>(&'a self, path: &'a str) -> Option<&'a str> {
        if self.root.is_empty() {
            Some(path)
        } else if path == self.root {
            Some("")
        } else if path.starts_with(&self.root) {
            let remainder = &path[self.root.len()..];
            if remainder.starts_with('/') {
                Some(remainder)
            } else {
                None
            }
        } else {
            None
        }
    }
}

impl<T, L> HandlerErased for RegisteredStruct<T, L>
where
    T: RepeStruct + 'static,
    L: Lockable<T> + 'static,
{
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        let path = req.query_str().unwrap_or("");
        let Some(relative) = self.relative_pointer(path) else {
            return Ok(create_error_response_like(
                req,
                ErrorCode::MethodNotFound,
                format!("path is not below struct root: {path}"),
            ));
        };

        let body = if req.body.is_empty() {
            None
        } else {
            match BodyFormat::try_from(req.header.body_format) {
                Ok(BodyFormat::Json) => {
                    Some(serde_json::from_slice::<Value>(&req.body).map_err(RepeError::from)?)
                }
                Ok(BodyFormat::Utf8) => {
                    Some(serde_json::from_str::<Value>(&req.body_utf8()).map_err(RepeError::from)?)
                }
                Ok(BodyFormat::Beve) => Some(beve_from_slice(&req.body)?),
                Ok(BodyFormat::RawBinary) | Err(_) => {
                    return Ok(create_error_response_like(
                        req,
                        ErrorCode::InvalidBody,
                        format!(
                            "struct handler `{path}` requires JSON or BEVE body, got format {}",
                            req.header.body_format
                        ),
                    ));
                }
            }
        };

        let mut guard = match self.shared.lock() {
            Ok(g) => g,
            Err(err) => {
                let detail = match err {
                    LockError::Poisoned(msg) => {
                        format!("struct handler `{path}` lock poisoned: {msg}")
                    }
                    LockError::Other(msg) => {
                        format!("struct handler `{path}` lock error: {msg}")
                    }
                };
                return Ok(create_error_response_like(
                    req,
                    ErrorCode::ParseError,
                    detail,
                ));
            }
        };

        dispatch_struct_segments(&mut *guard, relative, body, req)
    }
}

/// Run a `RepeStruct::repe_handle` call after parsing `relative` into
/// path segments and map the result back to a `Message`.
///
/// Segment parsing mirrors [`crate::json_pointer::parse`] byte-for-byte so the
/// dispatch result is independent of which path is taken:
///
/// * `""` → no reference tokens; calls `repe_handle(&[], _)` (whole struct).
/// * `"/"` → one empty reference token; calls `repe_handle(&[""], _)`. The
///   derive-macro impls treat this as `InvalidPath`, matching RFC 6901
///   semantics ("/" points at a field named `""`).
///
/// The escape-free fast path (`!relative.contains('~')`, the common case for
/// JSON Pointers) avoids the `Vec<String>` from `json_pointer::parse` and
/// drops segment `&str`s into a fixed-size stack buffer, spilling to a
/// `Vec<&str>` only when the path is unusually deep. The escape path keeps the
/// original `Vec<String>` + `Vec<&str>` shape because each escaped segment
/// genuinely needs an owned `String`.
fn dispatch_struct_segments<T>(
    handler: &mut T,
    relative: &str,
    body: Option<Value>,
    req: &Message,
) -> Result<Message, RepeError>
where
    T: RepeStruct + ?Sized,
{
    const STACK_SEGS: usize = 16;

    let result = if !relative.contains('~') {
        if relative.is_empty() {
            handler.repe_handle(&[], body)
        } else if relative == "/" {
            // RFC 6901: "/" decodes to a single empty reference token. The
            // old `json_pointer::parse("/")` path returned `vec![""]`; the
            // fast path must match so trailing-slash requests still surface
            // as `InvalidPath` rather than silently serving the whole struct.
            handler.repe_handle(&[""], body)
        } else {
            let trimmed = relative.strip_prefix('/').unwrap_or(relative);
            let mut stack: [&str; STACK_SEGS] = [""; STACK_SEGS];
            let mut count = 0usize;
            let mut overflow: Option<Vec<&str>> = None;
            for seg in trimmed.split('/') {
                if let Some(v) = overflow.as_mut() {
                    v.push(seg);
                } else if count < STACK_SEGS {
                    stack[count] = seg;
                    count += 1;
                } else {
                    let mut v = Vec::with_capacity(STACK_SEGS + 4);
                    v.extend_from_slice(&stack);
                    v.push(seg);
                    overflow = Some(v);
                }
            }
            match overflow.as_deref() {
                Some(v) => handler.repe_handle(v, body),
                None => handler.repe_handle(&stack[..count], body),
            }
        }
    } else {
        let owned = crate::json_pointer::parse(relative);
        let seg_refs: Vec<&str> = owned.iter().map(String::as_str).collect();
        handler.repe_handle(&seg_refs, body)
    };

    match result {
        Ok(value) => create_response_unstamped(req, value.unwrap_or(Value::Null), BodyFormat::Json),
        Err(err) => Ok(create_error_response_like(req, err.code(), err.to_string())),
    }
}

/// Pluggable trait for typed JSON handlers: auto-deserializes request JSON to `In`,
/// returns `Out` which is serialized to JSON response.
pub trait JsonTypedHandler: Send + Sync {
    type In: DeserializeOwned + Send + 'static;
    type Out: Serialize + Send + 'static;
    fn call(&self, input: Self::In) -> Result<Self::Out, (ErrorCode, String)>;
}

struct JsonTypedAdapter<H: JsonTypedHandler>(H);

impl<H: JsonTypedHandler> HandlerErased for JsonTypedAdapter<H> {
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        let t: H::In = match BodyFormat::try_from(req.header.body_format) {
            Ok(BodyFormat::Json) => serde_json::from_slice(&req.body)?,
            Ok(BodyFormat::Utf8) => {
                serde_json::from_str(&req.body_utf8()).map_err(RepeError::from)?
            }
            Ok(BodyFormat::Beve) => beve_from_slice(&req.body)?,
            _ => {
                return Ok(create_error_response_like(
                    req,
                    ErrorCode::InvalidBody,
                    "Expected JSON body",
                ));
            }
        };
        match self.0.call(t) {
            Ok(r) => create_response_unstamped(req, r, BodyFormat::Json),
            Err((code, msg)) => Ok(create_error_response_like(req, code, msg)),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub struct Server {
    router: Router,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    running: Arc<AtomicBool>,
    tcp_nodelay: bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl Server {
    pub fn new(router: Router) -> Self {
        Self {
            router,
            read_timeout: None,
            write_timeout: None,
            running: Arc::new(AtomicBool::new(false)),
            tcp_nodelay: true,
        }
    }

    pub fn read_timeout(mut self, d: Option<Duration>) -> Self {
        self.read_timeout = d;
        self
    }
    pub fn write_timeout(mut self, d: Option<Duration>) -> Self {
        self.write_timeout = d;
        self
    }

    /// Control whether accepted connections call `set_nodelay`.
    /// `true` disables Nagle's algorithm (the default); `false` leaves it enabled.
    pub fn tcp_nodelay(mut self, enabled: bool) -> Self {
        self.tcp_nodelay = enabled;
        self
    }

    pub fn listen<A: ToSocketAddrs>(&self, addr: A) -> std::io::Result<TcpListener> {
        TcpListener::bind(addr)
    }

    pub fn serve(self, listener: TcpListener) -> std::io::Result<()> {
        self.running.store(true, Ordering::SeqCst);
        listener.set_nonblocking(false)?;
        while self.running.load(Ordering::SeqCst) {
            let (stream, _addr) = match listener.accept() {
                Ok(p) => p,
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        continue;
                    }
                    return Err(e);
                }
            };
            let router = self.router.clone();
            let running = self.running.clone();
            let rt = self.read_timeout;
            let wt = self.write_timeout;
            let nodelay = self.tcp_nodelay;
            thread::spawn(move || {
                if let Err(e) = handle_connection(stream, router, running, rt, wt, nodelay) {
                    eprintln!("[repe] connection error: {e}");
                }
            });
        }
        Ok(())
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn handle_connection(
    stream: TcpStream,
    router: Router,
    running: Arc<AtomicBool>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    tcp_nodelay: bool,
) -> Result<(), RepeError> {
    stream.set_nodelay(tcp_nodelay)?;
    stream.set_read_timeout(read_timeout)?;
    stream.set_write_timeout(write_timeout)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    // One read buffer reused for every request on this connection, so steady-
    // state request framing allocates nothing: the frame is parsed as a borrowed
    // `MessageView` and the response echoes the query straight out of `buf`.
    let mut buf = Vec::new();
    while running.load(Ordering::SeqCst) {
        match read_message_into(&mut reader, &mut buf) {
            Ok(()) => {}
            Err(RepeError::Io(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        let view = MessageView::from_slice(&buf)?;
        if let Some(resp) = route_request_view(&router, &view) {
            // Echo the request query (a borrowed slice of `buf`) unless the
            // handler set its own — matching the owned path's stamp rule — so no
            // query buffer is copied into the response on the common path.
            let echo = crate::message::response_echo_query(&resp, view.query);
            write_message_streaming(&mut writer, resp.header, echo, resp.body.len() as u64, |w| {
                w.write_all(&resp.body)
            })?;
            writer.flush()?;
        }
    }
    Ok(())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::io::{read_message, write_message};
    use crate::message::create_response;
    use crate::{QueryFormat, REPE_VERSION};
    use serde::{Deserialize, Serialize};
    use std::io::Read;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    #[test]
    fn middleware_runs_for_registered_handlers() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_for_middleware = Arc::clone(&hits);
        let router = Router::new()
            .with_middleware(move |req: &Message, next: Next<'_>| {
                assert_eq!(req.query_str().unwrap(), "/echo");
                hits_for_middleware.fetch_add(1, Ordering::SeqCst);
                next.run(req)
            })
            .with_json("/echo", |v: Value| Ok(v));

        let request = Message::builder()
            .id(7)
            .query_str("/echo")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&serde_json::json!({"payload": 42}))
            .unwrap()
            .build();

        let handler = router.get("/echo").expect("handler to exist");
        let response = handler.handle(&request).unwrap();
        assert_eq!(response.header.ec, ErrorCode::Ok as u32);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn middleware_can_short_circuit_requests() {
        let handler_called = Arc::new(AtomicBool::new(false));
        let handler_called_inner = Arc::clone(&handler_called);

        let router = Router::new()
            .with_middleware(|req: &Message, _next: Next<'_>| {
                create_response(
                    req,
                    serde_json::json!({"status": "blocked"}),
                    BodyFormat::Json,
                )
            })
            .with_json("/blocked", move |_v: Value| {
                handler_called_inner.store(true, Ordering::SeqCst);
                Ok(serde_json::json!({"status": "allowed"}))
            });

        let request = Message::builder()
            .id(11)
            .query_str("/blocked")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&serde_json::json!({}))
            .unwrap()
            .build();

        let handler = router.get("/blocked").expect("handler to exist");
        let response = handler.handle(&request).unwrap();

        assert_eq!(response.header.ec, ErrorCode::Ok as u32);
        let body: serde_json::Value = response.json_body().unwrap();
        assert_eq!(body.get("status").and_then(|v| v.as_str()), Some("blocked"));
        assert!(!handler_called.load(Ordering::SeqCst));
    }

    #[test]
    fn typed_handler_accepts_beve_payload() {
        #[derive(Serialize, Deserialize, Debug)]
        struct DeviceMeta {
            id: String,
            location: String,
        }

        #[derive(Serialize, Deserialize, Debug)]
        struct SampleStream {
            channel: String,
            samples: Vec<f64>,
        }

        #[derive(Serialize, Deserialize, Debug)]
        struct SensorFrame {
            device: DeviceMeta,
            streams: Vec<SampleStream>,
            tags: std::collections::HashMap<String, String>,
        }

        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct Aggregate {
            device: String,
            location: String,
            sample_count: usize,
            average: f64,
            tags: std::collections::HashMap<String, String>,
        }

        let router = Router::new().with_typed::<SensorFrame, Aggregate, _>(
            "/aggregate",
            |frame: SensorFrame| {
                let mut total = 0.0;
                let mut count = 0usize;
                for stream in &frame.streams {
                    for value in &stream.samples {
                        total += *value;
                        count += 1;
                    }
                }
                let average = if count == 0 {
                    0.0
                } else {
                    total / count as f64
                };
                Ok(Aggregate {
                    device: frame.device.id,
                    location: frame.device.location,
                    sample_count: count,
                    average,
                    tags: frame.tags,
                })
            },
        );

        let mut tags = std::collections::HashMap::new();
        tags.insert("site".to_string(), "warehouse".to_string());
        tags.insert("line".to_string(), "A-1".to_string());

        let frame = SensorFrame {
            device: DeviceMeta {
                id: "sensor-42".into(),
                location: "north-wing".into(),
            },
            streams: vec![
                SampleStream {
                    channel: "temperature".into(),
                    samples: vec![20.5, 21.0, 20.7],
                },
                SampleStream {
                    channel: "humidity".into(),
                    samples: vec![47.0, 46.5],
                },
            ],
            tags: tags.clone(),
        };

        let expected_total: f64 = frame
            .streams
            .iter()
            .flat_map(|s| s.samples.iter())
            .copied()
            .sum();
        let expected_count: usize = frame.streams.iter().map(|s| s.samples.len()).sum();

        let request = Message::builder()
            .id(101)
            .query_str("/aggregate")
            .query_format(QueryFormat::JsonPointer)
            .body_beve(&frame)
            .unwrap()
            .build();

        let handler = router.get("/aggregate").expect("handler to exist");
        let response = handler.handle(&request).unwrap();

        assert_eq!(response.header.ec, ErrorCode::Ok as u32);
        assert_eq!(response.header.body_format, BodyFormat::Json as u16);
        let aggregate: Aggregate = response.json_body().unwrap();
        assert_eq!(aggregate.device, "sensor-42");
        assert_eq!(aggregate.location, "north-wing");
        assert_eq!(aggregate.sample_count, expected_count);
        assert!((aggregate.average - expected_total / expected_count as f64).abs() < 1e-12);
        assert_eq!(aggregate.tags, tags);
    }

    #[test]
    fn router_with_json_and_typed_handlers() {
        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct In {
            x: i32,
            y: i32,
        }
        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct Out {
            sum: i32,
        }

        let router = Router::new()
            .with_json("/echo", |v: Value| Ok(v))
            .with_typed::<In, Out, _>("/sum", |inp: In| Ok(Out { sum: inp.x + inp.y }));

        let msg = Message::builder()
            .id(1)
            .query_str("/sum")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&In { x: 2, y: 3 })
            .unwrap()
            .build();
        let h = router.get("/sum").expect("handler");
        let resp = h.handle(&msg).unwrap();
        assert_eq!(resp.header.ec, ErrorCode::Ok as u32);
        let out: Out = resp.json_body().unwrap();
        assert_eq!(out.sum, 5);
    }

    #[test]
    fn server_handle_connection_with_client() {
        // Router with a simple add method
        let router = Router::new().with_json("/add", |v: Value| {
            let a = v.get("a").and_then(|x| x.as_i64()).unwrap_or(0);
            let b = v.get("b").and_then(|x| x.as_i64()).unwrap_or(0);
            Ok(serde_json::json!({"sum": a + b}))
        });

        #[derive(Serialize, Deserialize)]
        struct AddReq {
            a: i64,
            b: i64,
        }

        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct AddResp {
            sum: i64,
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let router_clone = router.clone();
        let srv = thread::spawn(move || {
            let (stream, _addr) = listener.accept().unwrap();
            handle_connection(
                stream,
                router_clone,
                Arc::new(AtomicBool::new(true)),
                None,
                None,
                true,
            )
        });

        // Use the public Client API
        let client = crate::client::Client::connect(addr).unwrap();
        let out = client
            .call_json("/add", &serde_json::json!({"a": 3, "b": 4}))
            .unwrap();
        assert_eq!(out["sum"], 7);

        let typed: AddResp = client
            .call_typed_json("/add", &AddReq { a: 5, b: 6 })
            .unwrap();
        assert_eq!(typed.sum, 11);

        let beve: AddResp = client
            .call_typed_beve("/add", &AddReq { a: 1, b: 2 })
            .unwrap();
        assert_eq!(beve.sum, 3);

        // Close client to end the server loop and join
        drop(client);
        let _ = srv.join().unwrap();
    }

    #[test]
    fn beve_request_roundtrip_over_tcp() {
        #[derive(Serialize, Deserialize, Debug)]
        struct DeviceMeta {
            id: String,
            location: String,
        }

        #[derive(Serialize, Deserialize, Debug)]
        struct SampleStream {
            channel: String,
            samples: Vec<f64>,
        }

        #[derive(Serialize, Deserialize, Debug)]
        struct SensorFrame {
            device: DeviceMeta,
            streams: Vec<SampleStream>,
            alerts: Vec<String>,
        }

        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct Summary {
            device: String,
            max_sample: f64,
            average_sample: f64,
            alert_count: usize,
        }

        let router = Router::new().with_typed::<SensorFrame, Summary, _>(
            "/telemetry",
            |frame: SensorFrame| {
                let mut max_sample = f64::MIN;
                let mut total = 0.0f64;
                let mut count = 0usize;
                for stream in &frame.streams {
                    for value in &stream.samples {
                        if *value > max_sample {
                            max_sample = *value;
                        }
                        total += *value;
                        count += 1;
                    }
                }
                if count == 0 {
                    max_sample = 0.0;
                }
                let avg = if count == 0 {
                    0.0
                } else {
                    total / count as f64
                };
                let summary = Summary {
                    device: frame.device.id,
                    max_sample,
                    average_sample: avg,
                    alert_count: frame.alerts.len(),
                };
                Ok(TypedResponse::beve(summary))
            },
        );

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let router_clone = router.clone();
        let running = Arc::new(AtomicBool::new(true));
        let running_worker = running.clone();

        let srv = thread::spawn(move || {
            let (stream, _addr) = listener.accept().unwrap();
            handle_connection(stream, router_clone, running_worker, None, None, true).unwrap();
        });

        let frame = SensorFrame {
            device: DeviceMeta {
                id: "sensor-007".into(),
                location: "test-bench".into(),
            },
            streams: vec![
                SampleStream {
                    channel: "temp".into(),
                    samples: vec![34.2, 35.1, 36.0],
                },
                SampleStream {
                    channel: "vibration".into(),
                    samples: vec![0.2, 0.4, 0.3, 0.5],
                },
            ],
            alerts: vec![
                "overheat".to_string(),
                "door-open".to_string(),
                "power-cycle".to_string(),
            ],
        };

        let expected_samples: Vec<f64> = frame
            .streams
            .iter()
            .flat_map(|s| s.samples.iter())
            .copied()
            .collect();
        let expected_max = expected_samples
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);
        let expected_avg = expected_samples.iter().sum::<f64>() / expected_samples.len() as f64;

        let request = Message::builder()
            .id(314)
            .query_str("/telemetry")
            .query_format(QueryFormat::JsonPointer)
            .body_beve(&frame)
            .unwrap()
            .build();

        let socket = TcpStream::connect(addr).unwrap();
        socket.set_nodelay(true).ok();

        {
            let mut writer = BufWriter::new(socket.try_clone().unwrap());
            write_message(&mut writer, &request).unwrap();
            writer.flush().unwrap();
        }

        let mut reader = BufReader::new(socket);
        let response = read_message(&mut reader).unwrap();
        assert_eq!(response.header.id, 314);
        assert_eq!(response.header.body_format, BodyFormat::Beve as u16);
        assert_eq!(response.header.ec, ErrorCode::Ok as u32);

        let summary: Summary = response.beve_body().unwrap();
        assert_eq!(summary.device, "sensor-007");
        assert_eq!(summary.alert_count, 3);
        assert!((summary.max_sample - expected_max).abs() < 1e-12);
        assert!((summary.average_sample - expected_avg).abs() < 1e-12);

        running.store(false, Ordering::SeqCst);
        drop(reader);
        srv.join().unwrap();
    }

    #[test]
    fn unknown_route_returns_method_not_found() {
        let router = Router::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let router_clone = router.clone();
        let srv = thread::spawn(move || {
            let (stream, _addr) = listener.accept().unwrap();
            handle_connection(
                stream,
                router_clone,
                Arc::new(AtomicBool::new(true)),
                None,
                None,
                true,
            )
        });

        let client = crate::client::Client::connect(addr).unwrap();
        let err = client
            .call_json("/nope", &serde_json::json!({}))
            .unwrap_err();
        match err {
            RepeError::ServerError { code, message } => {
                assert_eq!(code, ErrorCode::MethodNotFound);
                assert!(message.contains("/nope"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
        drop(client);
        let _ = srv.join().unwrap();
    }

    #[test]
    fn version_mismatch_yields_error_response() {
        let router = Router::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let router_clone = router.clone();
        let srv = thread::spawn(move || {
            let (stream, _addr) = listener.accept().unwrap();
            handle_connection(
                stream,
                router_clone,
                Arc::new(AtomicBool::new(true)),
                None,
                None,
                true,
            )
        });

        let stream = TcpStream::connect(addr).unwrap();
        let mut req = Message::builder()
            .id(42)
            .query_str("/any")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&serde_json::json!({"x": 1}))
            .unwrap()
            .build();
        req.header.version = REPE_VERSION.wrapping_add(1);

        {
            let mut writer = BufWriter::new(stream.try_clone().unwrap());
            write_message(&mut writer, &req).unwrap();
            writer.flush().unwrap();
        }

        let mut reader = BufReader::new(stream);
        let resp = read_message(&mut reader).unwrap();
        assert_eq!(resp.header.id, 42);
        assert_eq!(resp.header.ec, ErrorCode::VersionMismatch as u32);
        assert!(
            resp.error_message_utf8()
                .unwrap()
                .contains("Unsupported REPE version")
        );

        drop(reader);
        let _ = srv.join().unwrap();
    }

    #[test]
    fn raw_binary_query_returns_invalid_query() {
        let router = Router::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let router_clone = router.clone();
        let srv = thread::spawn(move || {
            let (stream, _addr) = listener.accept().unwrap();
            handle_connection(
                stream,
                router_clone,
                Arc::new(AtomicBool::new(true)),
                None,
                None,
                true,
            )
        });

        let stream = TcpStream::connect(addr).unwrap();
        let req = Message::builder()
            .id(99)
            .query_bytes(vec![0, 1, 2])
            .query_format(QueryFormat::RawBinary)
            .body_json(&serde_json::json!({}))
            .unwrap()
            .build();

        {
            let mut writer = BufWriter::new(stream.try_clone().unwrap());
            write_message(&mut writer, &req).unwrap();
            writer.flush().unwrap();
        }

        let mut reader = BufReader::new(stream);
        let resp = read_message(&mut reader).unwrap();
        assert_eq!(resp.header.id, 99);
        assert_eq!(resp.header.ec, ErrorCode::InvalidQuery as u32);
        assert!(
            resp.error_message_utf8()
                .unwrap()
                .contains("Raw binary queries")
        );

        drop(reader);
        let _ = srv.join().unwrap();
    }

    #[test]
    fn handler_error_code_propagates() {
        let router = Router::new().with_json("/err", |_v: Value| {
            Err((ErrorCode::ApplicationErrorBase, "oops".into()))
        });
        // Build a request and invoke handler directly
        let msg = Message::builder()
            .id(2)
            .query_str("/err")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&serde_json::json!({}))
            .unwrap()
            .build();
        let h = router.get("/err").unwrap();
        let resp = h.handle(&msg).unwrap();
        assert_eq!(resp.header.ec, ErrorCode::ApplicationErrorBase as u32);
        assert_eq!(resp.error_message_utf8().as_deref(), Some("oops"));
    }

    #[test]
    fn typed_handler_bad_json_maps_to_parse_error_over_wire() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct NeedsI32 {
            a: i32,
        }
        let router = Router::new().with_typed::<NeedsI32, serde_json::Value, _>("/typed", |_inp| {
            Ok(serde_json::json!({"ok": true}))
        });

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let router_clone = router.clone();
        let srv = thread::spawn(move || {
            let (stream, _addr) = listener.accept().unwrap();
            handle_connection(
                stream,
                router_clone,
                Arc::new(AtomicBool::new(true)),
                None,
                None,
                true,
            )
        });

        let client = crate::client::Client::connect(addr).unwrap();
        // Provide invalid JSON for NeedsI32 (string instead of number)
        let err = client
            .call_json("/typed", &serde_json::json!({"a": "not-a-number"}))
            .unwrap_err();
        match err {
            RepeError::ServerError { code, .. } => assert_eq!(code, ErrorCode::ParseError),
            other => panic!("unexpected: {other:?}"),
        }
        drop(client);
        let _ = srv.join().unwrap();
    }

    #[derive(Clone)]
    struct Doubler;

    impl JsonTypedHandler for Doubler {
        type In = serde_json::Value;
        type Out = serde_json::Value;

        fn call(&self, input: Self::In) -> Result<Self::Out, (ErrorCode, String)> {
            let n = input
                .get("n")
                .and_then(|v| v.as_i64())
                .ok_or((ErrorCode::InvalidBody, "missing n".into()))?;
            Ok(serde_json::json!({"double": n * 2}))
        }
    }

    #[test]
    fn router_with_handler_invokes_typed_trait() {
        let router = Router::new().with_handler("/double", Doubler);
        let msg = Message::builder()
            .id(5)
            .query_str("/double")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&serde_json::json!({"n": 4}))
            .unwrap()
            .build();
        let handler = router.get("/double").expect("handler");
        let resp = handler.handle(&msg).unwrap();
        assert_eq!(resp.header.ec, ErrorCode::Ok as u32);
        let out: serde_json::Value = resp.json_body().unwrap();
        assert_eq!(out["double"], 8);
    }

    #[test]
    fn notify_requests_do_not_emit_responses() {
        let router = Router::new().with_json("/notify", |v: Value| Ok(v));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let runner = running.clone();
        let router_clone = router.clone();
        let srv = thread::spawn(move || {
            let (stream, _addr) = listener.accept().unwrap();
            handle_connection(stream, router_clone, runner, None, None, true)
        });

        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let notify = Message::builder()
            .id(1)
            .notify(true)
            .query_str("/notify")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&serde_json::json!({"ok": true}))
            .unwrap()
            .build();

        {
            let mut writer = BufWriter::new(stream.try_clone().unwrap());
            write_message(&mut writer, &notify).unwrap();
            writer.flush().unwrap();
        }

        thread::sleep(Duration::from_millis(50));
        let mut buf = [0u8; 1];
        match stream.read(&mut buf) {
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Ok(0) => {}
            other => panic!("unexpected read result: {other:?}"),
        }

        running.store(false, Ordering::SeqCst);
        drop(stream);
        srv.join().unwrap().unwrap();
    }

    #[test]
    fn plain_constructors_default_to_inline_execution() {
        let router = Router::new().with_json("/x", |_| Ok(serde_json::json!({})));
        assert_eq!(router.get("/x").unwrap().execution(), Execution::Inline);
    }

    #[test]
    fn blocking_constructors_mark_off_reader_execution() {
        #[derive(Deserialize)]
        struct In {
            n: i64,
        }
        // Named fns sidestep the higher-ranked-lifetime closure inference
        // quirk on the `&CallContext` parameter (it affects
        // `with_typed_ctx` too; it is not specific to the blocking
        // variant).
        fn typed(inp: In) -> Result<serde_json::Value, (ErrorCode, String)> {
            Ok(serde_json::json!({ "n": inp.n }))
        }
        fn typed_ctx(
            _ctx: &CallContext,
            inp: In,
        ) -> Result<serde_json::Value, (ErrorCode, String)> {
            Ok(serde_json::json!({ "n": inp.n }))
        }
        let router = Router::new()
            .with_json_blocking("/j", |_| Ok(serde_json::json!({})))
            .with_json_ctx_blocking("/jc", |_ctx, _v| Ok(serde_json::json!({})))
            .with_typed_blocking::<In, serde_json::Value, _>("/t", typed)
            .with_typed_ctx_blocking::<In, serde_json::Value, _>("/tc", typed_ctx);
        for path in ["/j", "/jc", "/t", "/tc"] {
            assert_eq!(
                router.get(path).unwrap().execution(),
                Execution::OffReader,
                "{path} should be off-reader"
            );
        }
    }

    #[test]
    fn middleware_preserves_off_reader_execution() {
        // Guards MiddlewarePipeline::execution() forwarding: a
        // registered middleware must not silently downgrade an
        // off-reader handler back to inline (which would reintroduce
        // the streaming deadlock).
        let router = Router::new()
            .with_middleware(|req: &Message, next: Next<'_>| next.run(req))
            .with_json_blocking("/x", |_| Ok(serde_json::json!({})));
        assert_eq!(router.get("/x").unwrap().execution(), Execution::OffReader);
    }

    #[test]
    fn blocking_route_behaves_like_inline_over_tcp() {
        // The _blocking constructors are WebSocket-only: the TCP server
        // never consults Execution, so a with_json_blocking route
        // round-trips exactly like with_json. Both TCP servers share the
        // route_request -> resolve/dispatch path; this exercises the
        // sync Server end to end.
        let router = Router::new().with_json_blocking("/echo", Ok);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let router_clone = router.clone();
        let srv = thread::spawn(move || {
            let (stream, _addr) = listener.accept().unwrap();
            handle_connection(
                stream,
                router_clone,
                Arc::new(AtomicBool::new(true)),
                None,
                None,
                true,
            )
        });

        let client = crate::client::Client::connect(addr).unwrap();
        let out = client
            .call_json("/echo", &serde_json::json!({ "n": 7 }))
            .unwrap();
        assert_eq!(out, serde_json::json!({ "n": 7 }));

        drop(client);
        let _ = srv.join().unwrap();
    }
}
