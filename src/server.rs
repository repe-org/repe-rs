use crate::constants::{BodyFormat, ErrorCode, HEADER_SIZE};
use crate::error::RepeError;
#[cfg(not(target_arch = "wasm32"))]
use crate::io::read_message_into;
use crate::message::{
    Message, MessageView, create_body_response_unstamped, create_error_response_like,
    create_error_response_unstamped_view, create_response_unstamped,
    create_response_unstamped_view, create_typed_slice_response_unstamped,
    create_typed_slice_response_unstamped_view,
};
use crate::peer::{CallContext, PeerHandle};
use crate::registry::Registry;
#[cfg(not(target_arch = "wasm32"))]
use crate::server_request::route_request_view;
use crate::structs::{RepeStruct, ResponseBody, StructResult};
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

    /// Forwarded, so wrapping a handler that overrides the borrowing path does
    /// not quietly cost it. Only the WebSocket server reads
    /// [`execution`](Self::execution), and it materializes an owned request
    /// before moving the call off the reader; the TCP and async servers take
    /// this path and are unaffected by the wrapper.
    fn handle_view(&self, view: &MessageView, ctx: &CallContext) -> Result<Message, RepeError> {
        self.0.handle_view(view, ctx)
    }

    fn execution(&self) -> Execution {
        Execution::OffReader
    }
}

/// Lets an already-erased handler be wrapped again — by
/// [`OffReaderHandler`], for [`Router::register_fallback_blocking`], which
/// receives an `Arc<dyn HandlerErased>` rather than a concrete type.
///
/// Every method forwards, `execution` included, so the wrapper decides.
impl HandlerErased for Arc<dyn HandlerErased> {
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        (**self).handle(req)
    }

    fn handle_with_ctx(&self, req: &Message, ctx: &CallContext) -> Result<Message, RepeError> {
        (**self).handle_with_ctx(req, ctx)
    }

    fn handle_view(&self, view: &MessageView, ctx: &CallContext) -> Result<Message, RepeError> {
        (**self).handle_view(view, ctx)
    }

    fn execution(&self) -> Execution {
        (**self).execution()
    }
}

fn decode_json_param(req: &Message) -> Result<Result<Value, Message>, RepeError> {
    let value: Value = match BodyFormat::try_from(req.header.body_format) {
        // JSON requires UTF-8, so a Utf8-framed body parses straight from the
        // bytes too — strict and allocation-free, matching the borrowing path.
        Ok(BodyFormat::Json) | Ok(BodyFormat::Utf8) => serde_json::from_slice(&req.body)?,
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
        Ok(BodyFormat::Json) | Ok(BodyFormat::Utf8) => serde_json::from_slice(&req.body)?,
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

/// Decode a BEVE typed-numeric-array request body into `Vec<T>` via the bulk
/// `read_typed_slice` path (one `copy_nonoverlapping`, no serde walk). The
/// typed-slice twin of [`decode_typed_param`].
///
/// Unlike the serde decoders, this accepts only [`BodyFormat::Beve`]: a typed
/// numeric array has no JSON/UTF-8 on-wire form, so any other body format is a
/// client error rather than something to coerce. A `Beve` body that is not a
/// typed array of `T` (wrong element class/width, or truncated) surfaces as a
/// [`RepeError::Beve`], matching how [`decode_typed_param`] propagates a serde
/// parse failure.
fn decode_typed_slice_param<T>(req: &Message) -> Result<Result<Vec<T>, Message>, RepeError>
where
    T: beve::BeveTypedSlice,
{
    match BodyFormat::try_from(req.header.body_format) {
        Ok(BodyFormat::Beve) => Ok(Ok(beve::read_typed_slice(&req.body)?)),
        _ => Ok(Err(create_error_response_like(
            req,
            ErrorCode::InvalidBody,
            "Expected BEVE typed-numeric body",
        ))),
    }
}

/// Borrowing twin of [`decode_typed_slice_param`]: bulk-decode straight from the
/// borrowed view body, with no owned `Message`.
fn decode_typed_slice_param_view<T>(
    view: &MessageView,
) -> Result<Result<Vec<T>, Message>, RepeError>
where
    T: beve::BeveTypedSlice,
{
    match BodyFormat::try_from(view.header.body_format) {
        Ok(BodyFormat::Beve) => Ok(Ok(beve::read_typed_slice(view.body)?)),
        _ => Ok(Err(create_error_response_unstamped_view(
            view,
            ErrorCode::InvalidBody,
            "Expected BEVE typed-numeric body",
        ))),
    }
}

/// Bulk numeric handler registered by [`Router::with_typed_slice`]: decodes a
/// contiguous `Vec<T>` request and frames a contiguous `Vec<R>` response, both
/// through BEVE's typed-slice fast path, bypassing serde's per-element walk on
/// the hot numeric path.
struct TypedSliceHandler<T, R, F>(F, std::marker::PhantomData<(T, R)>)
where
    T: beve::BeveTypedSlice + Send + Sync + 'static,
    R: beve::BeveTypedSlice + Send + Sync + 'static,
    F: Fn(Vec<T>) -> Result<Vec<R>, (ErrorCode, String)> + Send + Sync + 'static;

impl<T, R, F> HandlerErased for TypedSliceHandler<T, R, F>
where
    T: beve::BeveTypedSlice + Send + Sync + 'static,
    R: beve::BeveTypedSlice + Send + Sync + 'static,
    F: Fn(Vec<T>) -> Result<Vec<R>, (ErrorCode, String)> + Send + Sync + 'static,
{
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        let input: Vec<T> = match decode_typed_slice_param(req)? {
            Ok(v) => v,
            Err(err) => return Ok(err),
        };
        match (self.0)(input) {
            Ok(out) => Ok(create_typed_slice_response_unstamped(req, &out)),
            Err((code, msg)) => Ok(create_error_response_like(req, code, msg)),
        }
    }

    fn handle_view(&self, view: &MessageView, _ctx: &CallContext) -> Result<Message, RepeError> {
        let input: Vec<T> = match decode_typed_slice_param_view(view)? {
            Ok(v) => v,
            Err(err) => return Ok(err),
        };
        match (self.0)(input) {
            Ok(out) => Ok(create_typed_slice_response_unstamped_view(view, &out)),
            Err((code, msg)) => Ok(create_error_response_unstamped_view(view, code, msg)),
        }
    }
}

/// BEVE aligned-typed-array marker byte (`0x5C`): a typed array (type 4) in the
/// bool/string/aligned sub-category (3) with the aligned discriminator (2), per
/// BEVE spec §4. It opens the aligned wire form that
/// [`MessageBuilder::body_aligned_typed_slice`](crate::message::MessageBuilder::body_aligned_typed_slice)
/// emits and the borrowing route decodes. Kept as a local constant rather than reaching into BEVE's
/// header internals; [`aligned_marker_matches_beve`] pins it to BEVE's actual
/// output so a future BEVE change cannot drift past us silently.
const BEVE_ALIGNED_TYPED_ARRAY_MARKER: u8 = 0x5C;

/// A decoded typed-slice request body: either borrowed straight from the receive
/// buffer (the zero-copy win) or owned after a bulk copy (the fallback). Both
/// expose the payload as `&[T]`, so the handler closure is oblivious to which
/// path produced it.
enum SliceInput<'a, T> {
    Borrowed(&'a [T]),
    Owned(Vec<T>),
}

impl<T> SliceInput<'_, T> {
    #[inline]
    fn as_slice(&self) -> &[T] {
        match self {
            SliceInput::Borrowed(s) => s,
            SliceInput::Owned(v) => v,
        }
    }
}

/// Decode a typed-slice request body for the borrowing route, preferring a
/// zero-copy borrow and degrading gracefully.
///
/// Accepts both wire forms so a [`Router::with_typed_slice_ref`] route is a
/// drop-in superset of [`Router::with_typed_slice`]:
///
/// * An *aligned* typed array (marker `0x5C`, what `call_typed_slice_aligned`
///   sends) is borrowed as `&[T]` when the buffer permits, else bulk-copied (the
///   aligned payload is unaligned in this particular buffer). A genuine
///   element-type mismatch or truncation surfaces as an error from the owned
///   re-read.
/// * A *regular* typed array (what `call_typed_slice` / the serde path send) has
///   no padding to borrow through, so it is always bulk-copied via
///   [`beve::read_typed_slice`].
fn decode_typed_slice_ref_body<T>(body: &[u8]) -> Result<SliceInput<'_, T>, RepeError>
where
    T: beve::BeveTypedSlice,
{
    if body.first() == Some(&BEVE_ALIGNED_TYPED_ARRAY_MARKER) {
        match beve::read_aligned_typed_slice_ref::<T>(body) {
            Ok(slice) => Ok(SliceInput::Borrowed(slice)),
            // Borrow refused (buffer base not aligned, or big-endian target) or a
            // real decode error: re-read owned, which copies and either succeeds
            // or reports the definitive error.
            Err(_) => Ok(SliceInput::Owned(beve::read_aligned_typed_slice::<T>(
                body,
            )?)),
        }
    } else {
        Ok(SliceInput::Owned(beve::read_typed_slice::<T>(body)?))
    }
}

/// Body-format gate shared by the borrowing handler's owned and view paths: a
/// typed numeric array is only ever [`BodyFormat::Beve`], so any other format is
/// a client error rather than something to coerce (mirrors
/// [`decode_typed_slice_param`]). On the happy path it returns the decoded
/// [`SliceInput`]; on a wrong format it returns the prebuilt error `Message`.
fn decode_typed_slice_ref_param<'a, T>(
    body_format: u16,
    body: &'a [u8],
    on_bad_format: impl FnOnce() -> Message,
) -> Result<Result<SliceInput<'a, T>, Message>, RepeError>
where
    T: beve::BeveTypedSlice,
{
    match BodyFormat::try_from(body_format) {
        Ok(BodyFormat::Beve) => Ok(Ok(decode_typed_slice_ref_body::<T>(body)?)),
        _ => Ok(Err(on_bad_format())),
    }
}

/// Borrowing bulk numeric handler registered by [`Router::with_typed_slice_ref`].
/// Identical to [`TypedSliceHandler`] except the closure receives a borrowed
/// `&[T]` request, which on the view (server) path comes straight out of the
/// connection's receive buffer with no element copy when the aligned wire form
/// and buffer alignment allow it.
struct TypedSliceRefHandler<T, R, F>(F, std::marker::PhantomData<(T, R)>)
where
    T: beve::BeveTypedSlice + Send + Sync + 'static,
    R: beve::BeveTypedSlice + Send + Sync + 'static,
    F: Fn(&[T]) -> Result<Vec<R>, (ErrorCode, String)> + Send + Sync + 'static;

impl<T, R, F> HandlerErased for TypedSliceRefHandler<T, R, F>
where
    T: beve::BeveTypedSlice + Send + Sync + 'static,
    R: beve::BeveTypedSlice + Send + Sync + 'static,
    F: Fn(&[T]) -> Result<Vec<R>, (ErrorCode, String)> + Send + Sync + 'static,
{
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        let input =
            match decode_typed_slice_ref_param::<T>(req.header.body_format, &req.body, || {
                create_error_response_like(
                    req,
                    ErrorCode::InvalidBody,
                    "Expected BEVE typed-numeric body",
                )
            })? {
                Ok(v) => v,
                Err(err) => return Ok(err),
            };
        match (self.0)(input.as_slice()) {
            Ok(out) => Ok(create_typed_slice_response_unstamped(req, &out)),
            Err((code, msg)) => Ok(create_error_response_like(req, code, msg)),
        }
    }

    fn handle_view(&self, view: &MessageView, _ctx: &CallContext) -> Result<Message, RepeError> {
        let input =
            match decode_typed_slice_ref_param::<T>(view.header.body_format, view.body, || {
                create_error_response_unstamped_view(
                    view,
                    ErrorCode::InvalidBody,
                    "Expected BEVE typed-numeric body",
                )
            })? {
                Ok(v) => v,
                Err(err) => return Ok(err),
            };
        match (self.0)(input.as_slice()) {
            Ok(out) => Ok(create_typed_slice_response_unstamped_view(view, &out)),
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

/// A mount composed with the router's fallback: dispatch the mount, and when
/// the mount answers [`ErrorCode::MethodNotFound`], dispatch the fallback
/// instead of returning that miss to the client.
///
/// This is what [`Router::with_mount_fallthrough`] switches on. It is built at
/// registration time, so dispatch costs one `Arc::clone` out of the router and
/// one comparison of the mount's response code — there is no per-request
/// resolution work, and nothing extra is allocated on the hit path.
///
/// The composite is what the middleware pipeline wraps, not each half, so a
/// request that falls through runs the pipeline once rather than twice.
struct MountFallthrough {
    mount: Arc<dyn HandlerErased>,
    fallback: Arc<dyn HandlerErased>,
}

impl MountFallthrough {
    /// Run `dispatch` against the mount, and against the fallback instead when
    /// the mount answers `MethodNotFound`.
    ///
    /// The three [`HandlerErased`] entry points differ only in which method they
    /// call, so they share this rather than repeating the rule three times and
    /// leaving two of the copies for a future edit to miss.
    fn or_fallback(
        &self,
        dispatch: impl Fn(&dyn HandlerErased) -> Result<Message, RepeError>,
    ) -> Result<Message, RepeError> {
        // A mount frames its own miss as an `Ok` error response, so the code is
        // in the header; a handler that returns `Err` has the same code read out
        // of it, since the dispatch layer would frame the two identically a
        // moment later.
        let outcome = dispatch(self.mount.as_ref());
        let declined = match &outcome {
            Ok(response) => response.header.ec == ErrorCode::MethodNotFound as u32,
            Err(err) => err.to_error_code() == ErrorCode::MethodNotFound,
        };
        if declined {
            return dispatch(self.fallback.as_ref());
        }
        outcome
    }
}

impl HandlerErased for MountFallthrough {
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        self.or_fallback(|handler| handler.handle(req))
    }

    fn handle_with_ctx(&self, req: &Message, ctx: &CallContext) -> Result<Message, RepeError> {
        self.or_fallback(|handler| handler.handle_with_ctx(req, ctx))
    }

    fn handle_view(&self, view: &MessageView, ctx: &CallContext) -> Result<Message, RepeError> {
        self.or_fallback(|handler| handler.handle_view(view, ctx))
    }

    fn execution(&self) -> Execution {
        // Which half runs is not known until the mount has answered, and by
        // then the reader has already been committed. So the composite asks for
        // the stronger of the two: a blocking fallback keeps its off-reader
        // dispatch even behind a mount that would have run inline.
        match (self.mount.execution(), self.fallback.execution()) {
            (Execution::Inline, Execution::Inline) => Execution::Inline,
            _ => Execution::OffReader,
        }
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

    /// Run `f` under a *shared* guard, or report `None` if this lock has no
    /// shared mode.
    ///
    /// The two guard types have no common supertype — `RwLockReadGuard` is not
    /// a `RwLockWriteGuard` — so the borrow is handed to a closure rather than
    /// returned. `None` rather than a silent fallback to [`lock`](Self::lock):
    /// a mutex would then take the same lock the caller is about to take
    /// anyway, and the router needs to know not to try. `Self` is a generic
    /// parameter at every call site, so a mutex folds this to nothing.
    fn with_read<R, F>(&self, f: F) -> Option<Result<R, LockError>>
    where
        F: FnOnce(&T) -> R,
    {
        let _ = f;
        None
    }
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

    fn with_read<R, F>(&self, f: F) -> Option<Result<R, LockError>>
    where
        F: FnOnce(&T) -> R,
    {
        Some(
            std::sync::RwLock::read(self)
                .map_err(LockError::from)
                .map(|guard| f(&guard)),
        )
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

    fn with_read<R, F>(&self, f: F) -> Option<Result<R, LockError>>
    where
        F: FnOnce(&T) -> R,
    {
        Some(Ok(f(&self.blocking_read())))
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

    fn with_read<R, F>(&self, f: F) -> Option<Result<R, LockError>>
    where
        F: FnOnce(&T) -> R,
    {
        Some(Ok(f(&self.read())))
    }
}

#[derive(Clone)]
pub struct Router {
    inner: Arc<HashMap<String, RouterMapEntry>>,
    structs: Arc<Vec<StructEntry>>,
    registries: Arc<Vec<RegistryEntry>>,
    /// Handler for requests that match no registered route, if one was set.
    /// Resolved last by [`Router::get`], so it costs nothing on the hit path.
    fallback: Option<RouterMapEntry>,
    middlewares: Arc<Vec<Arc<dyn Middleware>>>,
    /// Whether a mount that would answer `MethodNotFound` defers to the
    /// fallback instead. Off by default; see
    /// [`with_mount_fallthrough`](Router::with_mount_fallthrough).
    mount_fallthrough: bool,
}

impl Router {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(HashMap::new()),
            structs: Arc::new(Vec::new()),
            registries: Arc::new(Vec::new()),
            fallback: None,
            middlewares: Arc::new(Vec::new()),
            mount_fallthrough: false,
        }
    }

    /// Build the dispatched form of a mount: composed with the fallback when
    /// [`with_mount_fallthrough`](Self::with_mount_fallthrough) is on, then
    /// wrapped in the active middleware pipeline.
    ///
    /// Every registrar that builds a [`RegistryEntry`] or a [`StructEntry`] goes
    /// through here, and so does every rebuild, which is what lets registration
    /// order not matter.
    fn mount_dispatched(&self, raw: &Arc<dyn HandlerErased>) -> Arc<dyn HandlerErased> {
        let composed = match (self.mount_fallthrough, &self.fallback) {
            (true, Some(fallback)) => Arc::new(MountFallthrough {
                mount: Arc::clone(raw),
                fallback: Arc::clone(&fallback.raw),
            }) as Arc<dyn HandlerErased>,
            _ => Arc::clone(raw),
        };
        wrap_with_middlewares(&composed, &self.middlewares)
    }

    /// Rebuild the `dispatched` slot of every mounted registry and struct.
    ///
    /// Run whenever an input to [`mount_dispatched`](Self::mount_dispatched)
    /// changes — the fallback, the middleware chain, or the fall-through flag —
    /// so a mount registered before any of them still ends up composed with it.
    fn rebuild_mount_dispatch(&mut self) {
        // Clone the entries and replace one field, rather than rebuilding each
        // from its parts: a field added to either entry type later is carried
        // through here without anyone having to remember this function exists.
        let mut registries = (*self.registries).clone();
        for entry in &mut registries {
            entry.dispatched = self.mount_dispatched(&entry.raw);
        }
        self.registries = Arc::new(registries);

        let mut structs = (*self.structs).clone();
        for entry in &mut structs {
            entry.dispatched = self.mount_dispatched(&entry.raw);
        }
        self.structs = Arc::new(structs);
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

    /// Register a custom [`HandlerErased`] at `path`, the low-level escape hatch
    /// beneath the typed/JSON registrars.
    ///
    /// The built-in `with_*` constructors decode the request body into a value
    /// and frame the response for you; this one hands you the raw request and
    /// lets you return any [`Message`], so you control the response query bytes,
    /// query format, and body format directly. That is what a protocol carried
    /// over REPE but not shaped like a single value-in/value-out call needs — for
    /// example a handler that returns a raw-binary body with a flag byte in the
    /// response query (see the `value-stream` feature, which is built entirely on
    /// this method).
    ///
    /// The handler participates in the middleware pipeline and the response
    /// query-echo rule exactly like the built-ins: a response left with an empty
    /// query has the request query echoed in by the dispatch layer, while a
    /// query the handler sets itself is preserved (see [`HandlerErased`]).
    ///
    /// Distinct from [`with_handler`](Self::with_handler), which adapts a
    /// [`JsonTypedHandler`] (value-in/value-out); this takes an already-erased
    /// [`HandlerErased`] for full control of the response frame.
    pub fn with_erased_handler(mut self, path: &str, handler: Arc<dyn HandlerErased>) -> Self {
        self.insert_route(path, handler);
        self
    }

    /// Serve requests that match no registered route with `handler`, instead of
    /// answering [`ErrorCode::MethodNotFound`].
    ///
    /// This is the hook for a routing table that does not exist yet when the
    /// router is built. `Router`'s registrars all run before
    /// [`Server::serve`](crate::server::Server::serve) — `with_*` consume
    /// `self`, `register_*` take `&mut self` — so a path discovered at runtime
    /// has nowhere to go. A fallback is where it goes: a plugin host that
    /// `dlopen`s libraries on demand and dispatches by their claimed
    /// `root_path` (see `repe::plugin::host`), a proxy to another node, a
    /// registry mounted after startup.
    ///
    /// It is resolved **last**, after the fixed routes, the mounted registries,
    /// and the mounted structs, so a static route is never slowed down by it. It
    /// is wrapped in the active middleware pipeline exactly like every other
    /// route, so middleware sees fallback-served requests too — which is more
    /// than middleware could do before, since a pipeline is attached per route
    /// and so never ran on a miss at all.
    ///
    /// The handler owns every miss it is given, including the paths it does not
    /// want: nothing else will answer them, so a handler that does not claim the
    /// request must frame [`ErrorCode::MethodNotFound`] itself.
    ///
    /// **A mount answers for its whole prefix, misses included.** A registry or
    /// struct mounted at `/x` frames its own `MethodNotFound` for `/x/absent`,
    /// and the fallback is never consulted — resolution stops at the first
    /// prefix that matches. A fallback sees only paths no mount covers, so a
    /// plugin whose root overlaps a mounted struct is shadowed by it. A mount at
    /// the *empty* root matches every path and so shadows the fallback
    /// completely; [`with_mount_fallthrough`](Self::with_mount_fallthrough)
    /// turns a mount's miss back into a miss, which is what that registration
    /// needs.
    ///
    /// ```
    /// use repe::{Message, QueryFormat, constants::ErrorCode, error::RepeError};
    /// use repe::message::create_error_response_like;
    /// use repe::server::{HandlerErased, Router};
    /// use std::sync::Arc;
    ///
    /// struct Dynamic;
    /// impl HandlerErased for Dynamic {
    ///     fn handle(&self, req: &Message) -> Result<Message, RepeError> {
    ///         let path = req.query_str().unwrap_or("");
    ///         if let Some(rest) = path.strip_prefix("/dynamic/") {
    ///             return Ok(Message::builder()
    ///                 .id(req.header.id)
    ///                 .body_json(&rest)?
    ///                 .build());
    ///         }
    ///         // Not ours: the router no longer frames this for us.
    ///         Ok(create_error_response_like(
    ///             req,
    ///             ErrorCode::MethodNotFound,
    ///             format!("Method not found: {path}"),
    ///         ))
    ///     }
    /// }
    ///
    /// let router = Router::new().with_fallback(Arc::new(Dynamic));
    /// assert!(router.get("/dynamic/anything").is_some());
    /// ```
    pub fn with_fallback(mut self, handler: Arc<dyn HandlerErased>) -> Self {
        self.register_fallback(handler);
        self
    }

    /// Register the miss handler in-place, replacing any previous one.
    ///
    /// See [`with_fallback`](Self::with_fallback) for what a fallback is and
    /// when it runs.
    pub fn register_fallback(&mut self, handler: Arc<dyn HandlerErased>) {
        let dispatched = wrap_with_middlewares(&handler, &self.middlewares);
        self.fallback = Some(RouterMapEntry {
            raw: handler,
            dispatched,
        });
        // Under `with_mount_fallthrough` the mounts hold a clone of the
        // fallback, so replacing it has to reach them. Unconditional, so the
        // invariant is the simple one — every input change rebuilds — at a cost
        // of one `Arc::clone` per mount at builder time.
        self.rebuild_mount_dispatch();
    }

    /// Off-reader variant of [`with_fallback`](Self::with_fallback): on the
    /// WebSocket server the handler runs on a blocking thread (see
    /// [`Execution::OffReader`]) so the reader stays free to decode further
    /// inbound frames while it runs or parks. On the TCP servers it behaves
    /// exactly like [`with_fallback`](Self::with_fallback).
    ///
    /// This is the one to reach for when the miss handler leaves the process —
    /// a plugin call across a C ABI, a proxied request to another node — since
    /// neither has a bound this server knows.
    pub fn with_fallback_blocking(mut self, handler: Arc<dyn HandlerErased>) -> Self {
        self.register_fallback_blocking(handler);
        self
    }

    /// In-place counterpart of
    /// [`with_fallback_blocking`](Self::with_fallback_blocking).
    pub fn register_fallback_blocking(&mut self, handler: Arc<dyn HandlerErased>) {
        self.register_fallback(Arc::new(OffReaderHandler(handler)));
    }

    /// Let a mounted registry or struct defer to the
    /// [`fallback`](Self::with_fallback) instead of framing
    /// [`ErrorCode::MethodNotFound`] itself.
    ///
    /// Without this, a mount answers for its whole prefix, misses included: a
    /// struct mounted at `/x` frames its own miss for `/x/absent` and the
    /// fallback never sees it. That is the right default for a mount at a
    /// prefix — the mount is the authority on what lives under its root — but it
    /// has a degenerate case. **A mount at the empty root matches every path**,
    /// so it shadows the fallback for every request the router will ever see,
    /// and the fallback becomes unreachable rather than merely narrowed. An
    /// object published at the top level is the ordinary shape for a service
    /// ported from Glaze's `glz::asio_server::on(*this)`, so this is reached by
    /// the common registration, not an exotic one.
    ///
    /// With it on, a mount that would answer `MethodNotFound` hands the request
    /// to the fallback instead. The rule is uniform — a mount's miss is still a
    /// miss, at the empty root or at any prefix — and it does not reorder
    /// anything: a mount that *does* serve the path still answers it, and a
    /// fixed route registered with [`with_json`](Self::with_json) and friends
    /// still wins over both.
    ///
    /// Registration order does not matter. The composition is rebuilt whenever
    /// the fallback, the middleware chain, or this flag changes, so
    /// `with_mount_fallthrough()` may come before or after the mounts and the
    /// fallback it affects. Middleware wraps the composite rather than each
    /// half, so a request that falls through runs the pipeline once.
    ///
    /// Three consequences worth stating:
    ///
    /// * **The fallback still owns every miss it is given**, exactly as it does
    ///   for a path no mount covers. What changes is which requests reach it,
    ///   not what it owes them.
    /// * **The mount's own diagnostic is replaced**, which is why this is
    ///   opt-in: `path is not below struct root` and the derive's `InvalidPath`
    ///   name the mount and the path, where the fallback's refusal is whatever
    ///   the fallback frames. A host that mounts only at prefixes may prefer the
    ///   more specific message.
    /// * **The trigger is the error code, not the reason.** Any answer a mount
    ///   frames as `MethodNotFound` falls through — including a registry
    ///   callable that deliberately returns that code, whose answer the fallback
    ///   then supersedes. A handler that means "this exists and refused you"
    ///   should say so with a different code.
    ///
    /// A mount's miss goes to the fallback, never to the next matching mount:
    /// resolution still stops at the first prefix that matches.
    ///
    /// ```
    /// use repe::{Message, QueryFormat, constants::ErrorCode, error::RepeError};
    /// use repe::message::create_error_response_like;
    /// use repe::server::{HandlerErased, Router};
    /// use repe::structs::{RepeStruct, StructError, StructResult};
    /// use std::sync::Arc;
    ///
    /// struct Root;
    /// impl RepeStruct for Root {
    ///     fn repe_handle(
    ///         &mut self,
    ///         segments: &[&str],
    ///         _body: Option<serde_json::Value>,
    ///     ) -> StructResult<Option<serde_json::Value>> {
    ///         match segments {
    ///             ["voltages"] => Ok(Some(serde_json::json!([1, 2, 3]))),
    ///             _ => Err(StructError::InvalidPath {
    ///                 path: repe::structs::path_from_segments(segments),
    ///             }),
    ///         }
    ///     }
    /// }
    ///
    /// struct Plugins;
    /// impl HandlerErased for Plugins {
    ///     fn handle(&self, req: &Message) -> Result<Message, RepeError> {
    ///         let path = req.query_str().unwrap_or("");
    ///         if path.starts_with("/plugins/") {
    ///             return Ok(Message::builder().id(req.header.id).body_json(&path)?.build());
    ///         }
    ///         Ok(create_error_response_like(
    ///             req,
    ///             ErrorCode::MethodNotFound,
    ///             format!("Method not found: {path}"),
    ///         ))
    ///     }
    /// }
    ///
    /// // The struct is mounted at the root, so it matches `/plugins/x` too.
    /// let (router, _root) = Router::new().with_struct("", Root);
    /// let router = router
    ///     .with_fallback(Arc::new(Plugins))
    ///     .with_mount_fallthrough();
    ///
    /// let ask = |path: &str| {
    ///     let request = Message::builder()
    ///         .id(1)
    ///         .query_str(path)
    ///         .query_format(QueryFormat::JsonPointer)
    ///         .build()
    ///         .to_vec();
    ///     Message::from_slice(&router.call(&request).unwrap()).unwrap()
    /// };
    ///
    /// // Served by the struct, as before.
    /// assert_eq!(ask("/voltages").header.ec, 0);
    /// // The struct has nothing here, so the fallback answers it.
    /// assert_eq!(ask("/plugins/audio").header.ec, 0);
    /// // And a path neither claims is still a miss.
    /// assert_eq!(ask("/nowhere").header.ec, ErrorCode::MethodNotFound as u32);
    /// ```
    pub fn with_mount_fallthrough(mut self) -> Self {
        self.set_mount_fallthrough(true);
        self
    }

    /// In-place counterpart of
    /// [`with_mount_fallthrough`](Self::with_mount_fallthrough), and the way to
    /// turn the behavior back off.
    ///
    /// `set_` rather than the `register_` the other in-place forms use: those
    /// add a handler, this flips a mode, and a `register_` that took a `bool`
    /// would read as registering one.
    pub fn set_mount_fallthrough(&mut self, enabled: bool) {
        if self.mount_fallthrough == enabled {
            return;
        }
        self.mount_fallthrough = enabled;
        self.rebuild_mount_dispatch();
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

        if let Some(entry) = &mut self.fallback {
            entry.dispatched = wrap_with_middlewares(&entry.raw, &self.middlewares);
        }

        self.rebuild_mount_dispatch();

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

    /// Add a bulk numeric handler over contiguous slices: decode a `Vec<T>`
    /// request and frame a `Vec<R>` response through BEVE's typed-slice fast
    /// path, where `T` and `R` are scalar numeric types ([`f32`], [`f64`],
    /// integer widths, ...).
    ///
    /// This is the high-throughput counterpart to [`with_typed`](Self::with_typed)
    /// for whole-body numeric arrays. `with_typed` routes `Vec<f64>` through
    /// serde, which visits every element on both decode and encode; `with_typed_slice`
    /// moves the whole contiguous block in a single bounds-checked
    /// `copy_nonoverlapping` each way (on little-endian targets), bypassing the
    /// per-element walk. The bytes on the wire are identical, so a `with_typed_slice`
    /// route interoperates freely with a serde peer and with
    /// [`AsyncClient::call_typed_slice`](crate::AsyncClient::call_typed_slice) /
    /// [`Client::call_typed_slice`](crate::Client::call_typed_slice).
    ///
    /// The request body must be a BEVE typed numeric array of `T` (what those
    /// client helpers and [`MessageBuilder::body_typed_slice`] produce); any
    /// other body format is rejected with [`ErrorCode::InvalidBody`].
    ///
    /// ```ignore
    /// use repe::server::Router;
    /// // Scale every sample by 2.0, fully on the bulk path.
    /// let router = Router::new()
    ///     .with_typed_slice::<f64, f64, _>("/scale", |xs| Ok(xs.iter().map(|x| x * 2.0).collect()));
    /// ```
    ///
    /// [`with_typed`]: Self::with_typed
    /// [`MessageBuilder::body_typed_slice`]: crate::message::MessageBuilder::body_typed_slice
    pub fn with_typed_slice<T, R, F>(mut self, path: &str, f: F) -> Self
    where
        T: beve::BeveTypedSlice + Send + Sync + 'static,
        R: beve::BeveTypedSlice + Send + Sync + 'static,
        F: Fn(Vec<T>) -> Result<Vec<R>, (ErrorCode, String)> + Send + Sync + 'static,
    {
        self.insert_route(
            path,
            Arc::new(TypedSliceHandler::<T, R, F>(f, std::marker::PhantomData)),
        );
        self
    }

    /// Add a bulk numeric handler whose closure *borrows* its request as `&[T]`,
    /// the zero-copy counterpart of [`with_typed_slice`](Self::with_typed_slice).
    ///
    /// When the client sends the request through the aligned wire form
    /// ([`AsyncClient::call_typed_slice_aligned`] / [`Client::call_typed_slice_aligned`])
    /// and the connection's receive buffer happens to be aligned to
    /// `align_of::<T>()` (the common case for the reused per-connection buffer on
    /// little-endian targets), the handler receives a `&[T]` pointing straight
    /// into that buffer: no allocation and no element copy on the way in. When the
    /// buffer is not aligned, or the client used the regular
    /// [`call_typed_slice`](crate::AsyncClient::call_typed_slice) / serde path, the
    /// body is bulk-copied into an owned buffer first and the handler sees a `&[T]`
    /// over that. Either way the closure is identical, so a `with_typed_slice_ref`
    /// route is a drop-in superset of `with_typed_slice`: it accepts every client
    /// the latter does, and additionally borrows when it can.
    ///
    /// The response is framed exactly as [`with_typed_slice`](Self::with_typed_slice)
    /// frames it (a regular typed array), so it interoperates with every client on
    /// the way back out.
    ///
    /// ```ignore
    /// use repe::server::Router;
    /// // Sum each request without ever copying it into a `Vec<f64>`.
    /// let router = Router::new()
    ///     .with_typed_slice_ref::<f64, f64, _>("/stats", |xs| Ok(vec![xs.iter().sum()]));
    /// ```
    ///
    /// [`with_typed_slice`]: Self::with_typed_slice
    /// [`AsyncClient::call_typed_slice_aligned`]: crate::AsyncClient::call_typed_slice_aligned
    /// [`Client::call_typed_slice_aligned`]: crate::Client::call_typed_slice_aligned
    pub fn with_typed_slice_ref<T, R, F>(mut self, path: &str, f: F) -> Self
    where
        T: beve::BeveTypedSlice + Send + Sync + 'static,
        R: beve::BeveTypedSlice + Send + Sync + 'static,
        F: Fn(&[T]) -> Result<Vec<R>, (ErrorCode, String)> + Send + Sync + 'static,
    {
        self.insert_route(
            path,
            Arc::new(TypedSliceRefHandler::<T, R, F>(f, std::marker::PhantomData)),
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
        let dispatched = self.mount_dispatched(&raw);
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

    /// [`with_struct`](Self::with_struct) behind an `RwLock`, so that reads
    /// share the guard.
    ///
    /// The default `Mutex` serializes every request against the object,
    /// including a pure field read. Under an `RwLock` a read is attempted
    /// through `&self` first — every field at any nesting depth, a listing with
    /// nothing to invoke, a `&self` method taking no arguments, and the getter
    /// half of a `&self` field-shaped endpoint — and only falls back to the
    /// exclusive guard when the struct declines. Writes and `&mut self` methods
    /// are unaffected.
    ///
    /// Prefer this for an object whose handlers can block: it is what keeps a
    /// `/version` read from queueing behind a long-running command.
    pub fn with_struct_rw<T>(mut self, root: &str, value: T) -> (Self, Arc<std::sync::RwLock<T>>)
    where
        T: RepeStruct + Send + Sync + 'static,
    {
        let shared = self.register_struct_rw(root, value);
        (self, shared)
    }

    /// [`with_struct_rw`](Self::with_struct_rw) in place, returning the shared
    /// handle without breaking builder chains.
    pub fn register_struct_rw<T>(&mut self, root: &str, value: T) -> Arc<std::sync::RwLock<T>>
    where
        T: RepeStruct + Send + Sync + 'static,
    {
        let shared = Arc::new(std::sync::RwLock::new(value));
        self.register_struct_shared::<T, std::sync::RwLock<T>>(root, Arc::clone(&shared));
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
        let dispatched = self.mount_dispatched(&raw);
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

    /// Resolve the handler that serves `path`: a fixed route, then a mounted
    /// registry or struct whose prefix covers it, then the
    /// [`fallback`](Self::with_fallback) if one is registered.
    ///
    /// `None` means the request is a `MethodNotFound`. Registering a fallback
    /// therefore makes this return `Some` for every path, which is the point of
    /// one: the fallback decides what it does not serve.
    ///
    /// Under [`with_mount_fallthrough`](Self::with_mount_fallthrough) a mount's
    /// deferral to the fallback happens *inside* the handler this returns, not
    /// here — resolution still stops at the first prefix that matches, and the
    /// handler it yields is the mount composed with the fallback.
    pub fn get(&self, path: &str) -> Option<Arc<dyn HandlerErased>> {
        if let Some(entry) = self.inner.get(path) {
            return Some(Arc::clone(&entry.dispatched));
        }
        if let Some(entry) = self.registries.iter().find(|entry| entry.matches(path)) {
            return Some(Arc::clone(&entry.dispatched));
        }
        if let Some(entry) = self.structs.iter().find(|entry| entry.matches(path)) {
            return Some(Arc::clone(&entry.dispatched));
        }
        self.fallback
            .as_ref()
            .map(|entry| Arc::clone(&entry.dispatched))
    }

    /// Dispatch one serialized REPE request frame and return the serialized
    /// response frame, with no transport involved.
    ///
    /// This is the router reduced to its essential shape — bytes in, bytes out —
    /// and it is the same work the built-in servers do between reading a frame
    /// off a socket and writing one back: version and query-format validation,
    /// handler resolution, notify semantics, `MethodNotFound` framing, and the
    /// response query echo. Reaching those through [`get`](Self::get) plus
    /// [`HandlerErased::handle`] means reimplementing all five, and the notify
    /// and query-echo rules in particular are easy to get subtly wrong.
    ///
    /// Returns `None` when the request is a notify, which by protocol produces
    /// no response at all. A caller that must answer every frame should treat
    /// `None` as "write nothing", not as an error.
    ///
    /// Use it for any carrier the crate does not ship a server for: the C-ABI
    /// [`plugin`](mod@crate::plugin) surface (where it is literally the body of
    /// `repe_plugin_call`), a shared-memory or in-process transport, a foreign
    /// event loop, or a test that wants to exercise routing without a socket.
    ///
    /// ```
    /// use repe::{Message, QueryFormat, server::Router};
    ///
    /// let router = Router::new().with_json("/double", |v: serde_json::Value| {
    ///     Ok(serde_json::json!(v.as_i64().unwrap_or(0) * 2))
    /// });
    ///
    /// let request = Message::builder()
    ///     .id(1)
    ///     .query_str("/double")
    ///     .query_format(QueryFormat::JsonPointer)
    ///     .body_json(&21)
    ///     .unwrap()
    ///     .build()
    ///     .to_vec();
    ///
    /// let response = Message::from_slice(&router.call(&request).unwrap()).unwrap();
    /// assert_eq!(response.json_body::<i64>().unwrap(), 42);
    /// ```
    #[cfg(not(target_arch = "wasm32"))]
    pub fn call(&self, request: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        self.call_into(request, &mut out).then_some(out)
    }

    /// [`call`](Self::call) writing into a caller-owned buffer instead of
    /// allocating one, returning `true` if a response was written and `false`
    /// for a notify. `out` is cleared first either way.
    ///
    /// This is the form a hot loop wants: a carrier that holds one reusable
    /// response buffer per connection or per thread pays no per-request
    /// allocation, which is exactly why the C-ABI plugin surface is built on it
    /// rather than on [`call`](Self::call).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn call_into(&self, request: &[u8], out: &mut Vec<u8>) -> bool {
        out.clear();
        // `from_slice_exact`, not `from_slice`: one frame per buffer is the
        // contract here (the caller states the length), so trailing bytes mean
        // the caller miscounted. Surfacing that beats silently serving a prefix.
        match MessageView::from_slice_exact(request) {
            Ok(view) => {
                let Some(response) = route_request_view(self, &view) else {
                    return false;
                };
                // The `expect` is not a swallowed error: `Vec<u8>`'s `Write`
                // impl returns `Ok` unconditionally, so there is no reachable
                // failure to propagate.
                crate::server_request::write_view_response(out, &response, view.query)
                    .expect("`Vec<u8>` as a `Write` sink is infallible");
                true
            }
            Err(err) => {
                // The frame did not parse, so neither the request id nor the
                // notify flag is knowable. Answer with an id-0 error frame,
                // which is what Glaze's `plugin_error_response` emits for the
                // same condition. Staying silent on the chance it was a notify
                // would strand a caller that was in fact awaiting a response,
                // and an id of 0 is the honest statement that the id was
                // unreadable.
                //
                // This one carries its own (empty) query, so it writes itself.
                crate::message::create_error_message(err.to_error_code(), err.to_string())
                    .write_to(out)
                    .expect("`Vec<u8>` as a `Write` sink is infallible");
                true
            }
        }
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
        Self {
            prefix: crate::registry::normalize_mount(prefix),
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

        // A read — a frame with no body, by REPE's own read/write distinction —
        // may be servable through a shared borrow, which is the whole point of
        // registering the struct behind an `RwLock`. A mutex answers `None`
        // from a defaulted method with no work in it, so it compiles out.
        if req.body.is_empty() {
            match self
                .shared
                .with_read(|handler| read_struct_segments(handler, relative, req))
            {
                Some(Ok(Some(response))) => return Ok(response),
                // The struct declined: this path needs `&mut self`. Nothing was
                // written, so the exclusive attempt below starts from scratch.
                Some(Ok(None)) => {}
                Some(Err(err)) => return Ok(lock_error_response(req, path, err)),
                // This lock has no shared mode.
                None => {}
            }
        }

        let body = if req.body.is_empty() {
            None
        } else {
            match BodyFormat::try_from(req.header.body_format) {
                Ok(BodyFormat::Json) | Ok(BodyFormat::Utf8) => {
                    Some(serde_json::from_slice::<Value>(&req.body).map_err(RepeError::from)?)
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
            Err(err) => return Ok(lock_error_response(req, path, err)),
        };

        Ok(dispatch_struct_segments(&mut *guard, relative, body, req))
    }
}

/// The response to a lock that could not be taken, shared by the exclusive and
/// shared paths so a poisoned lock reads the same either way.
fn lock_error_response(req: &Message, path: &str, err: LockError) -> Message {
    let detail = match err {
        LockError::Poisoned(msg) => format!("struct handler `{path}` lock poisoned: {msg}"),
        LockError::Other(msg) => format!("struct handler `{path}` lock error: {msg}"),
    };
    create_error_response_like(req, ErrorCode::ParseError, detail)
}

/// Parse `relative` into JSON Pointer segments and hand them to `f`.
///
/// Segment parsing mirrors [`crate::json_pointer::parse`] byte-for-byte so the
/// dispatch result is independent of which path is taken:
///
/// * `""` → no reference tokens; calls `f(&[])` (whole struct).
/// * `"/"` → one reference token, empty; calls `f(&[""])`. The derive-macro
///   impls treat this as `InvalidPath`, matching RFC 6901 semantics ("/" points
///   at a field named `""`).
///
/// The escape-free fast path (`!relative.contains('~')`, the common case for
/// JSON Pointers) avoids the `Vec<String>` from `json_pointer::parse` and drops
/// segment `&str`s into a fixed-size stack buffer, spilling to a `Vec<&str>`
/// only when the path is unusually deep. The escape path keeps the original
/// `Vec<String>` + `Vec<&str>` shape because each escaped segment genuinely
/// needs an owned `String`.
fn with_segments<R, F>(relative: &str, f: F) -> R
where
    F: FnOnce(&[&str]) -> R,
{
    const STACK_SEGS: usize = 16;

    if relative.contains('~') {
        let owned = crate::json_pointer::parse(relative);
        let seg_refs: Vec<&str> = owned.iter().map(String::as_str).collect();
        return f(&seg_refs);
    }
    if relative.is_empty() {
        return f(&[]);
    }
    if relative == "/" {
        // RFC 6901: "/" decodes to a single empty reference token. The old
        // `json_pointer::parse("/")` path returned `vec![""]`; the fast path
        // must match so trailing-slash requests still surface as `InvalidPath`
        // rather than silently serving the whole struct.
        return f(&[""]);
    }

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
        Some(v) => f(v),
        None => f(&stack[..count]),
    }
}

/// A response buffer sized for one leaf read.
///
/// The frame needs the wire prefix regardless, and a scalar or short-string leaf
/// read — the case the encode-in-place path exists for — fits in the rest, so
/// the encode comes for free out of the one allocation and the result already
/// satisfies [`Message::into_wire_bytes`]'s in-place condition
/// (`capacity >= HEADER_SIZE + query + body`).
fn response_buffer(req: &Message) -> Vec<u8> {
    /// Body allowance reserved on top of the wire prefix.
    const LEAF_BODY_HINT: usize = 64;
    Vec::with_capacity(HEADER_SIZE + req.query.len() + LEAF_BODY_HINT)
}

/// Run a `RepeStruct::repe_handle_into` call against an exclusive borrow and map
/// the result back to a `Message`.
///
/// The handler encodes through [`RepeStruct::repe_handle_into`], writing the
/// response body straight into the buffer rather than returning a
/// [`serde_json::Value`] for this function to re-serialize. The buffer is then
/// grown by the wire prefix so [`Message::into_wire_bytes`] reuses it instead of
/// allocating a frame, making a leaf read one allocation end to end.
fn dispatch_struct_segments<T>(
    handler: &mut T,
    relative: &str,
    body: Option<Value>,
    req: &Message,
) -> Message
where
    T: RepeStruct + ?Sized,
{
    let mut buf = response_buffer(req);
    let mut out = ResponseBody::new(&mut buf);
    let result = with_segments(relative, |segments| {
        handler.repe_handle_into(segments, body, &mut out)
    });
    let body_format = out.format();
    finish_struct_response(req, buf, body_format, result)
}

/// The shared-borrow counterpart: attempt the same read through
/// [`RepeStruct::repe_read_into`], and report `None` if the struct declined
/// because the path needs exclusive access.
fn read_struct_segments<T>(handler: &T, relative: &str, req: &Message) -> Option<Message>
where
    T: RepeStruct + ?Sized,
{
    let mut buf = response_buffer(req);
    let mut out = ResponseBody::new(&mut buf);
    let result = with_segments(relative, |segments| {
        handler.repe_read_into(segments, &mut out)
    })?;
    let body_format = out.format();
    Some(finish_struct_response(req, buf, body_format, result))
}

/// Turn what a dispatch left in `buf` into the response frame, shared by the
/// exclusive and shared paths so an error reads the same either way.
fn finish_struct_response(
    req: &Message,
    buf: Vec<u8>,
    body_format: BodyFormat,
    result: StructResult<()>,
) -> Message {
    match result {
        Ok(()) => create_body_response_unstamped(req, buf, body_format),
        Err(err) => create_error_response_like(req, err.code(), err.to_string()),
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
            Ok(BodyFormat::Json) | Ok(BodyFormat::Utf8) => serde_json::from_slice(&req.body)?,
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
            crate::server_request::write_view_response(&mut writer, &resp, view.query)?;
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

    /// Pin [`BEVE_ALIGNED_TYPED_ARRAY_MARKER`] to BEVE's actual aligned-array
    /// output, so the borrowing route's marker dispatch can never silently drift
    /// from the format BEVE emits.
    #[test]
    fn aligned_marker_matches_beve() {
        let encoded = beve::to_vec_aligned_typed_slice(&[1.0_f64]);
        assert_eq!(encoded.first(), Some(&BEVE_ALIGNED_TYPED_ARRAY_MARKER));
    }

    /// The borrowing decoder's owned fallback: a well-formed aligned body whose
    /// payload is not aligned in this buffer (here, forced by placing it at an odd
    /// offset) must refuse the zero-copy borrow and bulk-copy instead, still
    /// returning the correct data. This is the path a `body_aligned_typed_slice`
    /// body padded for the wrong frame offset (e.g. query set after the body) takes
    /// on the server, so the documented graceful degradation is correctness-safe.
    #[test]
    fn ref_decode_falls_back_to_owned_when_payload_unaligned() {
        let data: Vec<f64> = (0..16).map(|i| i as f64 * 3.0).collect();
        let aligned = beve::to_vec_aligned_typed_slice(&data);

        // Place the aligned body at offset 1 in a genuinely 8-aligned backing
        // buffer (`Vec<u64>`), so its DATA pointer is deterministically odd and the
        // zero-copy borrow must refuse regardless of allocator behavior.
        let words = aligned.len() / 8 + 2;
        let mut backing: Vec<u64> = vec![0; words];
        // SAFETY: `backing` is 8-aligned and large enough to hold `aligned` at +1.
        let bytes =
            unsafe { std::slice::from_raw_parts_mut(backing.as_mut_ptr() as *mut u8, words * 8) };
        bytes[1..1 + aligned.len()].copy_from_slice(&aligned);
        let body = &bytes[1..1 + aligned.len()];

        let decoded = decode_typed_slice_ref_body::<f64>(body).expect("decode");
        assert!(
            matches!(decoded, SliceInput::Owned(_)),
            "unaligned aligned body must take the owned fallback"
        );
        assert_eq!(decoded.as_slice(), data.as_slice());

        // And the regular (unaligned wire) typed array always decodes owned too.
        let regular = beve::to_vec_typed_slice(&data);
        let decoded = decode_typed_slice_ref_body::<f64>(&regular).expect("decode regular");
        assert_eq!(decoded.as_slice(), data.as_slice());
    }

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
    // ---------------------------------------------------------------------
    // `Router::call` / `Router::call_into` — transport-free dispatch
    // ---------------------------------------------------------------------

    fn call_request(path: &str, body: &serde_json::Value, notify: bool) -> Vec<u8> {
        Message::builder()
            .id(11)
            .notify(notify)
            .query_str(path)
            .query_format(crate::constants::QueryFormat::JsonPointer)
            .body_json(body)
            .unwrap()
            .build()
            .to_vec()
    }

    fn echo_router() -> Router {
        Router::new().with_json("/echo", Ok)
    }

    #[test]
    fn call_round_trips_id_query_and_body() {
        let response = echo_router()
            .call(&call_request(
                "/echo",
                &serde_json::json!({ "n": 3 }),
                false,
            ))
            .expect("a non-notify request produces a response");
        let message = Message::from_slice(&response).unwrap();
        assert_eq!(message.header.id, 11);
        assert_eq!(message.query_str().unwrap(), "/echo");
        assert_eq!(
            message.json_body::<serde_json::Value>().unwrap(),
            serde_json::json!({ "n": 3 })
        );
    }

    #[test]
    fn call_returns_nothing_for_a_notify() {
        // The distinction `get` + `handle` cannot make on its own, and the one
        // a hand-written carrier most often gets wrong.
        assert!(
            echo_router()
                .call(&call_request("/echo", &serde_json::json!(1), true))
                .is_none()
        );
    }

    #[test]
    fn call_frames_method_not_found_rather_than_returning_none() {
        let response = echo_router()
            .call(&call_request("/absent", &serde_json::json!(1), false))
            .expect("an unknown method is still answered");
        let message = Message::from_slice(&response).unwrap();
        assert_eq!(message.error_code(), Some(ErrorCode::MethodNotFound));
        assert_eq!(message.header.id, 11, "the id is echoed even on an error");
    }

    #[test]
    fn call_answers_an_unparseable_frame_with_an_id_zero_error() {
        let response = echo_router()
            .call(b"too short to be a header")
            .expect("a malformed frame is answered, not swallowed");
        let message = Message::from_slice(&response).unwrap();
        assert!(message.is_error());
        assert_eq!(message.header.id, 0, "the id could not be read");
    }

    #[test]
    fn call_rejects_a_buffer_carrying_more_than_one_frame() {
        let mut wire = call_request("/echo", &serde_json::json!(1), false);
        wire.extend_from_slice(b"trailing");
        let response = echo_router().call(&wire).expect("answered");
        assert!(
            Message::from_slice(&response).unwrap().is_error(),
            "trailing bytes mean the caller miscounted; serving the prefix would hide it"
        );
    }

    #[test]
    fn call_into_clears_the_buffer_between_requests() {
        // The reuse contract the plugin surface depends on: a carrier holding one
        // buffer per thread must not see the previous response bleed through.
        let router = echo_router();
        let mut out = Vec::new();

        // A long payload first, then a short one: if the buffer were not cleared,
        // the tail of the long response would trail the short one.
        assert!(router.call_into(
            &call_request("/echo", &serde_json::json!("a".repeat(64)), false),
            &mut out
        ));

        assert!(router.call_into(
            &call_request("/echo", &serde_json::json!("short"), false),
            &mut out
        ));
        let message = Message::from_slice(&out).unwrap();
        assert_eq!(message.json_body::<String>().unwrap(), "short");
        assert_eq!(
            out.len() as u64,
            message.header.length,
            "the buffer holds exactly the second response and no residue"
        );

        // A notify leaves the buffer empty rather than stale, so a carrier that
        // writes `out` whenever it is non-empty cannot resend the last response.
        assert!(!router.call_into(
            &call_request("/echo", &serde_json::json!(0), true),
            &mut out
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn call_preserves_a_response_query_the_handler_set_itself() {
        // The echo rule: an empty response query means "echo the request's", but a
        // handler that sets one keeps it. Reimplementing this per carrier is
        // exactly what `call` exists to prevent.
        struct Rerouting;
        impl HandlerErased for Rerouting {
            fn handle(&self, req: &Message) -> Result<Message, RepeError> {
                let mut response = create_error_response_like(req, ErrorCode::Ok, "");
                response.header.ec = 0;
                response.query = b"/elsewhere".to_vec();
                Ok(response)
            }
        }

        let router = Router::new().with_erased_handler("/here", Arc::new(Rerouting));
        let response = router
            .call(&call_request("/here", &serde_json::json!(null), false))
            .unwrap();
        assert_eq!(
            Message::from_slice(&response).unwrap().query_str().unwrap(),
            "/elsewhere"
        );
    }
    // ---------------------------------------------------------------------
    // `Router::with_fallback` — dispatch for routes that do not exist yet
    // ---------------------------------------------------------------------

    /// Answers anything under `/dynamic`, and frames `MethodNotFound` for the
    /// rest — the shape a plugin host has, where the claimed prefix is known
    /// only at run time.
    struct DynamicPrefix;

    impl HandlerErased for DynamicPrefix {
        fn handle(&self, req: &Message) -> Result<Message, RepeError> {
            let path = req.query_str().unwrap_or("").to_string();
            if path == "/dynamic" || path.starts_with("/dynamic/") {
                return Ok(Message::builder()
                    .id(req.header.id)
                    .body_json(&path)
                    .unwrap()
                    .build());
            }
            Ok(create_error_response_like(
                req,
                ErrorCode::MethodNotFound,
                format!("Method not found: {path}"),
            ))
        }
    }

    /// Counts what reaches it and answers with an empty body. Used to prove a
    /// fallback was, or was not, consulted.
    struct Counting(Arc<AtomicUsize>);

    impl HandlerErased for Counting {
        fn handle(&self, req: &Message) -> Result<Message, RepeError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Message::builder().id(req.header.id).build())
        }
    }

    fn served_path(router: &Router, path: &str) -> Message {
        let response = router
            .call(&call_request(path, &serde_json::json!(null), false))
            .expect("a non-notify request produces a response");
        Message::from_slice(&response).unwrap()
    }

    #[test]
    fn a_fallback_serves_a_path_no_route_claims() {
        let router = echo_router().with_fallback(Arc::new(DynamicPrefix));
        let message = served_path(&router, "/dynamic/thing");
        assert!(!message.is_error());
        assert_eq!(message.json_body::<String>().unwrap(), "/dynamic/thing");
    }

    #[test]
    fn a_fallback_does_not_shadow_a_registered_route() {
        // Resolution order is the whole design: a fallback is a miss handler,
        // not an override. `/echo` must still reach the route that claims it.
        let router = echo_router().with_fallback(Arc::new(DynamicPrefix));
        let message = served_path(&router, "/echo");
        assert!(!message.is_error());
        assert_eq!(
            message.json_body::<serde_json::Value>().unwrap(),
            serde_json::json!(null)
        );
    }

    #[test]
    fn a_fallback_does_not_shadow_a_mounted_registry_or_struct() {
        #[derive(Default)]
        struct Counter {
            value: i64,
        }

        impl RepeStruct for Counter {
            fn repe_handle(
                &mut self,
                segments: &[&str],
                _body: Option<Value>,
            ) -> crate::structs::StructResult<Option<Value>> {
                assert_eq!(segments, ["value"]);
                Ok(Some(serde_json::json!(self.value)))
            }
        }

        let registry = Arc::new(Registry::new());
        registry
            .register_value("/flag", serde_json::json!(true))
            .unwrap();

        let router = Router::new()
            .with_registry("/reg", registry)
            .with_struct_shared::<Counter, _>(
                "/counter",
                Arc::new(Mutex::new(Counter { value: 5 })),
            )
            .with_fallback(Arc::new(DynamicPrefix));

        assert!(!served_path(&router, "/reg/flag").is_error());
        assert_eq!(
            served_path(&router, "/counter/value")
                .json_body::<i64>()
                .unwrap(),
            5
        );
        assert_eq!(
            served_path(&router, "/dynamic/x")
                .json_body::<String>()
                .unwrap(),
            "/dynamic/x"
        );
    }

    #[test]
    fn a_fallback_frames_its_own_method_not_found() {
        // Once a fallback is registered the router never frames the miss itself,
        // so the handler owns the paths it declines as well as the ones it takes.
        let router = echo_router().with_fallback(Arc::new(DynamicPrefix));
        let message = served_path(&router, "/nothing/here");
        assert_eq!(message.error_code(), Some(ErrorCode::MethodNotFound));
        assert_eq!(message.header.id, 11);
    }

    #[test]
    fn a_fallback_honors_notify_semantics() {
        let router = echo_router().with_fallback(Arc::new(DynamicPrefix));
        assert!(
            router
                .call(&call_request("/dynamic/x", &serde_json::json!(null), true))
                .is_none()
        );
    }

    #[test]
    fn middleware_wraps_the_fallback_whichever_order_they_are_registered() {
        struct Blocker;
        impl Middleware for Blocker {
            fn handle(&self, req: &Message, _next: Next<'_>) -> Result<Message, RepeError> {
                Ok(create_error_response_like(
                    req,
                    ErrorCode::Timeout,
                    "blocked",
                ))
            }
        }

        for router in [
            Router::new()
                .with_middleware(Blocker)
                .with_fallback(Arc::new(DynamicPrefix)),
            Router::new()
                .with_fallback(Arc::new(DynamicPrefix))
                .with_middleware(Blocker),
        ] {
            assert_eq!(
                served_path(&router, "/dynamic/x").error_code(),
                Some(ErrorCode::Timeout),
                "the fallback is wrapped in the pipeline like any other route"
            );
        }
    }

    #[test]
    fn a_delegating_middleware_chain_still_reaches_the_fallback() {
        // The half a short-circuiting middleware cannot show: that the pipeline
        // *terminates* at the fallback handler. This is what breaks if the
        // rebuild in `register_middleware` ever wraps `dispatched` instead of
        // `raw`, or drops the fallback's `raw` on the way through.
        struct Tag(&'static str, Arc<Mutex<Vec<&'static str>>>);
        impl Middleware for Tag {
            fn handle(&self, req: &Message, next: Next<'_>) -> Result<Message, RepeError> {
                self.1.lock().unwrap().push(self.0);
                next.run(req)
            }
        }

        let seen = Arc::new(Mutex::new(Vec::new()));
        let router = Router::new()
            .with_middleware(Tag("outer", Arc::clone(&seen)))
            .with_fallback(Arc::new(DynamicPrefix))
            .with_middleware(Tag("inner", Arc::clone(&seen)));

        let message = served_path(&router, "/dynamic/reached");
        assert!(!message.is_error());
        assert_eq!(
            message.json_body::<String>().unwrap(),
            "/dynamic/reached",
            "the chain ran through to the fallback handler"
        );
        assert_eq!(*seen.lock().unwrap(), ["outer", "inner"]);
    }

    #[test]
    fn a_miss_inside_a_mounted_prefix_never_reaches_the_fallback() {
        // The limit of the feature, and the thing most likely to surprise: a
        // mounted registry or struct answers for its whole prefix, misses
        // included. A fallback only sees paths no mount covers, so a plugin
        // whose root overlaps a mounted struct is shadowed by it.
        let registry = Arc::new(Registry::new());
        registry
            .register_value("/flag", serde_json::json!(true))
            .unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .with_registry("/reg", registry)
            .with_fallback(Arc::new(Counting(Arc::clone(&hits))));

        // A read, not a write: a registry *write* to an unknown pointer creates it.
        let read = Message::builder()
            .id(11)
            .query_str("/reg/nope")
            .query_format(crate::constants::QueryFormat::JsonPointer)
            .build()
            .to_vec();
        let message = Message::from_slice(&router.call(&read).unwrap()).unwrap();
        assert_eq!(message.error_code(), Some(ErrorCode::MethodNotFound));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "the mount framed its own miss; the fallback was never consulted"
        );
    }

    // ---------------------------------------------------------------------
    // `Router::with_mount_fallthrough` — a mount's miss is still a miss
    // ---------------------------------------------------------------------

    /// A struct published at the top level, the shape a service ported from
    /// Glaze's `glz::asio_server::on(*this)` arrives in. It serves `/value` and
    /// refuses everything else the way the derive does.
    #[derive(Default)]
    struct RootObject;

    impl RepeStruct for RootObject {
        fn repe_handle(
            &mut self,
            segments: &[&str],
            _body: Option<Value>,
        ) -> crate::structs::StructResult<Option<Value>> {
            match segments {
                ["value"] => Ok(Some(serde_json::json!(7))),
                _ => Err(crate::structs::StructError::InvalidPath {
                    path: crate::structs::path_from_segments(segments),
                }),
            }
        }
    }

    fn root_mounted_router() -> Router {
        Router::new().with_struct_shared::<RootObject, _>("", Arc::new(Mutex::new(RootObject)))
    }

    #[test]
    fn an_empty_root_mount_shadows_the_fallback_completely() {
        // The degenerate case of "a mount answers for its whole prefix": an
        // empty root matches every path, so without fall-through the fallback is
        // not merely narrowed, it is unreachable.
        let hits = Arc::new(AtomicUsize::new(0));
        let router = root_mounted_router().with_fallback(Arc::new(Counting(Arc::clone(&hits))));
        assert!(served_path(&router, "/dynamic/x").is_error());
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn mount_fallthrough_hands_an_empty_root_mount_s_miss_to_the_fallback() {
        let router = root_mounted_router()
            .with_fallback(Arc::new(DynamicPrefix))
            .with_mount_fallthrough();

        // Still the mount's, and answered by it.
        assert_eq!(
            served_path(&router, "/value").json_body::<i64>().unwrap(),
            7
        );
        // Not the mount's, so the fallback gets it.
        assert_eq!(
            served_path(&router, "/dynamic/x")
                .json_body::<String>()
                .unwrap(),
            "/dynamic/x"
        );
        // Neither claims it, and the fallback still owns the refusal.
        assert_eq!(
            served_path(&router, "/nowhere").error_code(),
            Some(ErrorCode::MethodNotFound)
        );
    }

    #[test]
    fn mount_fallthrough_does_not_reorder_fixed_routes_over_mounts() {
        // The rule is "a mount's miss is a miss", not "the fallback wins": a
        // fixed route still beats the mount, and the mount still beats the
        // fallback for a path it serves.
        let mut router = root_mounted_router();
        router.insert_route("/echo", Arc::new(JsonHandler(Ok)));
        let router = router
            .with_fallback(Arc::new(DynamicPrefix))
            .with_mount_fallthrough();

        assert!(!served_path(&router, "/echo").is_error());
        assert_eq!(
            served_path(&router, "/value").json_body::<i64>().unwrap(),
            7
        );
    }

    #[test]
    fn mount_fallthrough_applies_whichever_order_it_is_registered() {
        // Every input to the composition — the mounts, the fallback, the flag —
        // can arrive in any order, because each one rebuilds the mounts.
        let build_in_order = |order: u8| {
            let mut router = Router::new();
            for step in 0..3u8 {
                match (order + step) % 3 {
                    0 => {
                        router.register_struct_shared::<RootObject, _>(
                            "",
                            Arc::new(Mutex::new(RootObject)),
                        );
                    }
                    1 => router.register_fallback(Arc::new(DynamicPrefix)),
                    _ => router.set_mount_fallthrough(true),
                }
            }
            router
        };

        for order in 0..3u8 {
            let router = build_in_order(order);
            assert_eq!(
                served_path(&router, "/dynamic/x")
                    .json_body::<String>()
                    .unwrap(),
                "/dynamic/x",
                "registration order {order} did not compose the fallback in"
            );
            assert_eq!(
                served_path(&router, "/value").json_body::<i64>().unwrap(),
                7
            );
        }
    }

    #[test]
    fn mount_fallthrough_defers_a_registry_miss_too() {
        // Not struct-specific: a registry mounted at a prefix defers the same way.
        let registry = Arc::new(Registry::new());
        registry
            .register_value("/flag", serde_json::json!(true))
            .unwrap();
        let router = Router::new()
            .with_registry("/reg", registry)
            .with_fallback(Arc::new(DynamicPrefix))
            .with_mount_fallthrough();

        assert!(!served_path(&router, "/reg/flag").is_error());
        // A read, not a write: a registry write to an unknown pointer creates it.
        let read = Message::builder()
            .id(11)
            .query_str("/reg/nope")
            .query_format(crate::constants::QueryFormat::JsonPointer)
            .build()
            .to_vec();
        let message = Message::from_slice(&router.call(&read).unwrap()).unwrap();
        assert_eq!(
            message.error_code(),
            Some(ErrorCode::MethodNotFound),
            "the fallback answered, and it refuses this path too"
        );
    }

    #[test]
    fn mount_fallthrough_runs_the_middleware_pipeline_once() {
        // Middleware wraps the composite rather than each half, so a request
        // that falls through is not billed twice for the pipeline.
        struct CountingMiddleware(Arc<AtomicUsize>);
        impl Middleware for CountingMiddleware {
            fn handle(&self, req: &Message, next: Next<'_>) -> Result<Message, RepeError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                next.run(req)
            }
        }

        let runs = Arc::new(AtomicUsize::new(0));
        let router = root_mounted_router()
            .with_fallback(Arc::new(DynamicPrefix))
            .with_mount_fallthrough()
            .with_middleware(CountingMiddleware(Arc::clone(&runs)));

        assert_eq!(
            served_path(&router, "/dynamic/x")
                .json_body::<String>()
                .unwrap(),
            "/dynamic/x"
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn mount_fallthrough_defers_through_every_dispatch_entry_point() {
        // `Router::call` takes the borrowing `handle_view` path, so it is the
        // only one of the three the tests above exercise. The owned entry points
        // are what the WebSocket server's off-reader path and any hand-written
        // carrier reach, and they have to defer identically.
        let router = root_mounted_router()
            .with_fallback(Arc::new(DynamicPrefix))
            .with_mount_fallthrough();
        let handler = router
            .get("/dynamic/x")
            .expect("the mount matches the root");
        let request =
            Message::from_slice(&call_request("/dynamic/x", &serde_json::json!(null), false))
                .unwrap();

        for (entry, response) in [
            ("handle", handler.handle(&request).unwrap()),
            (
                "handle_with_ctx",
                handler
                    .handle_with_ctx(&request, &CallContext::detached("/dynamic/x"))
                    .unwrap(),
            ),
        ] {
            assert_eq!(
                response.json_body::<String>().unwrap(),
                "/dynamic/x",
                "`{entry}` did not reach the fallback"
            );
        }
    }

    #[test]
    fn a_blocking_fallback_keeps_its_off_reader_dispatch_behind_a_mount() {
        // The composite cannot know which half will run until the mount has
        // answered, by which time the reader is committed — so it asks for the
        // stronger of the two.
        let router = root_mounted_router()
            .with_fallback_blocking(Arc::new(DynamicPrefix))
            .with_mount_fallthrough();
        assert_eq!(
            router.get("/dynamic/x").unwrap().execution(),
            Execution::OffReader
        );
    }

    #[test]
    fn set_mount_fallthrough_false_gives_the_prefix_back_to_the_mount() {
        let mut router = root_mounted_router()
            .with_fallback(Arc::new(DynamicPrefix))
            .with_mount_fallthrough();
        assert!(!served_path(&router, "/dynamic/x").is_error());

        router.set_mount_fallthrough(false);
        assert!(
            served_path(&router, "/dynamic/x").is_error(),
            "the mount owns its whole prefix again"
        );
    }

    #[test]
    fn mount_fallthrough_without_a_fallback_changes_nothing() {
        let router = root_mounted_router().with_mount_fallthrough();
        assert_eq!(
            served_path(&router, "/value").json_body::<i64>().unwrap(),
            7
        );
        assert_eq!(
            served_path(&router, "/dynamic/x").error_code(),
            Some(ErrorCode::MethodNotFound),
            "there is nothing to defer to, so the mount frames the miss"
        );
    }

    #[test]
    fn a_blocking_fallback_dispatches_off_the_reader() {
        let router = Router::new().with_fallback_blocking(Arc::new(DynamicPrefix));
        assert_eq!(
            router.get("/dynamic/x").unwrap().execution(),
            Execution::OffReader
        );
        // And it still serves: the wrapper delegates rather than replacing.
        assert_eq!(
            served_path(&router, "/dynamic/x")
                .json_body::<String>()
                .unwrap(),
            "/dynamic/x"
        );
    }

    #[test]
    fn registering_a_fallback_twice_replaces_the_first() {
        let mut router = echo_router();
        router.register_fallback(Arc::new(DynamicPrefix));
        router.register_fallback(Arc::new(JsonHandler(|_: Value| {
            Ok(serde_json::json!("second"))
        })));
        assert_eq!(
            served_path(&router, "/dynamic/x")
                .json_body::<String>()
                .unwrap(),
            "second"
        );
    }
}
