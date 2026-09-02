//! Dynamic endpoints resolved by JSON Pointer at run time.
//!
//! # What this is, and what it stopped being
//!
//! This used to be two things: a table of callables **and** a schemaless
//! document store — a `serde_json::Value` tree that
//! `register_value("/counter", json!(0))` created data in, and that reads and
//! writes navigated by pointer. The store is gone.
//!
//! It went with `serde_json::Value` itself, and the reason is not only that
//! structio has no tree to put it in. Glaze, which serves the same protocol,
//! has no analog: you own a struct and call `on(obj)`. A value registered here
//! had no Rust type at all, so nothing could describe it, nothing could validate
//! it, and every read of it cost a parse into a tree and a re-encode out of one.
//! Owning the data is what [`RepeStruct`](crate::RepeStruct) is for, and a
//! struct mounted on a [`Router`](crate::server::Router) serves the same paths
//! with a type behind each of them.
//!
//! So what remains is the half that was always about *behaviour* rather than
//! storage: a flat map from canonical pointer to a callable. That is Glaze's
//! shape too — one hash lookup on the whole query string, then the handler.
//!
//! # Migrating a registered value
//!
//! Declare it as a field and mount the struct:
//!
//! ```ignore
//! // Before: registry.register_value("/counter", json!(0))?;
//! #[derive(Default, repe::RepeStruct)]
//! struct State { counter: i64 }
//! structio::object!(State { counter });
//!
//! let (router, state) = router.with_struct("", State::default());
//! ```
//!
//! A value that genuinely has no type — a passthrough blob — is a function that
//! returns it.

use crate::constants::{BodyFormat, ErrorCode};
use crate::message::Message;
use crate::peer::CallContext;
use crate::structs::{RequestBody, ResponseBody};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

type RegistryFunction = Arc<dyn RegistryCallable>;

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("invalid JSON pointer `{pointer}`")]
    InvalidPointer { pointer: String },
    #[error("path not found `{path}`")]
    PathNotFound { path: String },
    #[error("registry body format `{format}` is unsupported")]
    UnsupportedBodyFormat { format: u16 },
    #[error("{message}")]
    Execution { code: ErrorCode, message: String },
}

impl RegistryError {
    pub fn code(&self) -> ErrorCode {
        match self {
            RegistryError::InvalidPointer { .. } | RegistryError::PathNotFound { .. } => {
                ErrorCode::MethodNotFound
            }
            RegistryError::UnsupportedBodyFormat { .. } => ErrorCode::InvalidBody,
            RegistryError::Execution { code, .. } => *code,
        }
    }
}

/// A callable the registry dispatches to.
///
/// The signature mirrors [`RepeStruct::repe_handle_into`](crate::RepeStruct::repe_handle_into),
/// and deliberately: a handler reads its parameters straight out of the frame
/// and writes its result straight into the outgoing one, with no value
/// materialized in between. `params` is `None` for a bodiless frame.
///
/// Handlers receive a [`CallContext`] so that peer-aware ones can push notify
/// messages back to the calling peer. A handler that does not care about the
/// context can be a plain closure: the blanket impl below ignores it.
///
/// # Returning a value
///
/// The blanket impls accept the ergonomic form — a closure returning something
/// writable — and do the `out.write(..)` for you:
///
/// ```ignore
/// registry.register_function("/double", |params: Option<RequestBody<'_>>| {
///     let n: i64 = params
///         .ok_or((ErrorCode::InvalidBody, "body required".to_string()))?
///         .read("/double")
///         .map_err(|err| (ErrorCode::InvalidBody, err.to_string()))?;
///     Ok(n * 2)
/// })?;
/// ```
///
/// Implement the trait directly when a handler wants the response buffer — to
/// stream a large body, or to answer with a BEVE typed array through
/// [`ResponseBody::write_typed_slice`].
///
/// To register a closure that *does* want the context, wrap it with
/// [`WithContext`]:
///
/// ```ignore
/// registry.register_function("/run", WithContext(|ctx: &CallContext, _params| {
///     if let Some(peer) = ctx.peer() {
///         peer.send_notify("/progress", NotifyBody::Json(b"{}".to_vec())).ok();
///     }
///     Ok(Status { status: "ok" })
/// }))?;
/// ```
pub trait RegistryCallable: Send + Sync {
    fn call(
        &self,
        ctx: &CallContext,
        params: Option<RequestBody<'_>>,
        out: &mut ResponseBody<'_>,
    ) -> Result<(), (ErrorCode, String)>;
}

impl<F, R> RegistryCallable for F
where
    F: for<'a> Fn(Option<RequestBody<'a>>) -> Result<R, (ErrorCode, String)>
        + Send
        + Sync
        + 'static,
    R: repe_core::structs::ServableWrite,
{
    fn call(
        &self,
        _ctx: &CallContext,
        params: Option<RequestBody<'_>>,
        out: &mut ResponseBody<'_>,
    ) -> Result<(), (ErrorCode, String)> {
        out.write(&(self)(params)?);
        Ok(())
    }
}

/// Newtype wrapper that adapts a context-aware closure into a
/// [`RegistryCallable`].
///
/// Rust's coherence rules forbid two blanket impls on different `Fn`
/// signatures for the same trait, so context-aware closures need a marker.
/// `WithContext` is that marker; pass it to
/// [`Registry::register_function`] like any other handler.
pub struct WithContext<F>(pub F);

impl<F, R> RegistryCallable for WithContext<F>
where
    F: for<'a> Fn(&CallContext, Option<RequestBody<'a>>) -> Result<R, (ErrorCode, String)>
        + Send
        + Sync
        + 'static,
    R: repe_core::structs::ServableWrite,
{
    fn call(
        &self,
        ctx: &CallContext,
        params: Option<RequestBody<'_>>,
        out: &mut ResponseBody<'_>,
    ) -> Result<(), (ErrorCode, String)> {
        out.write(&(self.0)(ctx, params)?);
        Ok(())
    }
}

/// Dynamic registry of callable entries, resolved by JSON pointer.
///
/// One flat map keyed on the whole pointer, so dispatch is a single hash lookup
/// — the same shape as Glaze's registry, and the same shape the struct router
/// already had.
///
/// Every registered path is a function. A frame with a body calls it with the
/// body as parameters; a frame without one calls it with `None`. There is no
/// third case, because there are no stored values left to read or write.
/// Newtype wrapper for a handler that writes its own response body.
///
/// The two closure impls above cover a handler that *returns* a value, which
/// the registry then encodes. A handler that needs the body itself — to stream
/// a large result, or to answer with
/// [`ResponseBody::write_typed_slice`](repe_core::structs::ResponseBody::write_typed_slice)
/// — takes `out` instead, and this is how such a closure is spelled. Without it
/// the only route is a hand-written `RegistryCallable` impl on a `Send + Sync`
/// struct, which is a cliff for what is otherwise a closure.
///
/// Wraps a context-free handler; a context-aware one that also writes its own
/// body implements [`RegistryCallable`] directly, which at three parameters is
/// no longer the cliff it was at one.
pub struct WithBody<F>(pub F);

impl<F> RegistryCallable for WithBody<F>
where
    F: for<'a> Fn(
            Option<RequestBody<'a>>,
            &mut ResponseBody<'_>,
        ) -> Result<(), (ErrorCode, String)>
        + Send
        + Sync
        + 'static,
{
    fn call(
        &self,
        _ctx: &CallContext,
        params: Option<RequestBody<'_>>,
        out: &mut ResponseBody<'_>,
    ) -> Result<(), (ErrorCode, String)> {
        (self.0)(params, out)
    }
}

#[derive(Default)]
pub struct Registry {
    /// Callables keyed by their canonical JSON Pointer string, computed once at
    /// registration. Keying on the `String` rather than on parsed segments lets
    /// dispatch probe the map with a borrowed `&str`: a wire pointer already
    /// *is* its own canonical key, so a call resolves without allocating.
    state: RwLock<HashMap<String, RegistryFunction>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a callable at `path`.
    pub fn register_function(
        &self,
        path: &str,
        function: impl RegistryCallable + 'static,
    ) -> Result<(), RegistryError> {
        self.register_function_arc(path, Arc::new(function))
    }

    pub fn register_function_arc(
        &self,
        path: &str,
        function: Arc<dyn RegistryCallable>,
    ) -> Result<(), RegistryError> {
        // A registration path may be given without its leading slash; every
        // other spelling question is settled by `canonical_key`, which the
        // lookup side also goes through, so the two cannot disagree.
        let normalized: Cow<'_, str> = if path.starts_with('/') {
            Cow::Borrowed(path)
        } else {
            Cow::Owned(format!("/{path}"))
        };
        if crate::json_pointer::addresses_root(&normalized) {
            return Err(invalid(path));
        }
        let key = canonical_key(&normalized)
            .map_err(|_| invalid(path))?
            .to_string();
        self.write_state().insert(key, function);
        Ok(())
    }

    /// Whether `pointer` names a registered function.
    ///
    /// Every registered path is one, so this is "is anything mounted here" —
    /// which is what a caller that has to commit to a verb before it has a body
    /// actually needs. A pointer that is malformed, or that names nothing, is
    /// not a function, so this answers `false` rather than raising: callers use
    /// it to pick a branch, and the branch they pick reports the real error.
    pub fn is_function(&self, pointer: &str) -> bool {
        match canonical_key(pointer) {
            Ok(key) => self.read_state().contains_key(key),
            Err(_) => false,
        }
    }

    /// Every registered pointer, in no particular order.
    ///
    /// The surface *is* the key set now that there is no tree to walk, which is
    /// what the REST gateway used to reconstruct by descending the document.
    pub fn endpoints(&self) -> Vec<String> {
        self.read_state().keys().cloned().collect()
    }

    /// Invoke the function at `pointer`, writing its result into `out`.
    ///
    /// A pointer naming nothing is [`PathNotFound`](RegistryError::PathNotFound).
    pub fn call(
        &self,
        pointer: &str,
        params: Option<RequestBody<'_>>,
        ctx: &CallContext,
        out: &mut ResponseBody<'_>,
    ) -> Result<(), RegistryError> {
        let key = canonical_key(pointer)?;
        let function = {
            let state = self.read_state();
            state.get(key).cloned()
        };
        let Some(function) = function else {
            return Err(RegistryError::PathNotFound {
                path: pointer.to_string(),
            });
        };
        function
            .call(ctx, params, out)
            .map_err(|(code, message)| RegistryError::Execution { code, message })
    }

    /// [`call`](Self::call) with a [`CallContext::detached`] context, for a
    /// direct in-process dispatch where there is no calling peer.
    ///
    /// This was `dispatch`, and it had three outcomes: read a value, write a
    /// value, or call a function, chosen from the body. Two of those are gone
    /// with the value tree, so it is a call either way and the name says so.
    pub fn call_detached(
        &self,
        pointer: &str,
        params: Option<RequestBody<'_>>,
        out: &mut ResponseBody<'_>,
    ) -> Result<(), RegistryError> {
        let ctx = CallContext::detached(pointer);
        self.call(pointer, params, &ctx, out)
    }

    /// Borrow a request's body as parameters, rejecting a format code this
    /// build does not recognize.
    ///
    /// Returns `None` for an empty body — the bodiless frame a no-argument call
    /// arrives on. Nothing is parsed here: a [`RequestBody`] is the frame's
    /// bytes plus the format its header declared, and the handler decides what
    /// to read them as.
    ///
    /// All four known formats pass, [`BodyFormat::RawBinary`] included. A
    /// handler reading one of those as a value gets it through the JSON parser,
    /// which is the reader `RequestBody` falls back to; a handler that means to
    /// treat it as bytes calls [`RequestBody::bytes`] and never parses it.
    pub fn body_of(req: &Message) -> Result<Option<RequestBody<'_>>, RegistryError> {
        if req.body.is_empty() {
            return Ok(None);
        }
        match BodyFormat::try_from(req.header.body_format) {
            Ok(format) => Ok(Some(RequestBody::new(&req.body, format))),
            // A code this build does not recognize. The spec can add one, and
            // handing a handler bytes under a format it cannot name is worse
            // than refusing.
            Err(_) => Err(RegistryError::UnsupportedBodyFormat {
                format: req.header.body_format,
            }),
        }
    }

    fn read_state(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, RegistryFunction>> {
        match self.state.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, RegistryFunction>> {
        match self.state.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Wrap a JSON Pointer syntax failure as this module's error, naming the
/// pointer the caller passed in.
fn invalid(pointer: &str) -> RegistryError {
    RegistryError::InvalidPointer {
        pointer: pointer.to_string(),
    }
}

/// The canonical JSON-Pointer key used to probe the registry's table.
///
/// Escaping and unescaping are inverse on every valid pointer, so the canonical
/// form of one *is* the pointer itself, byte for byte — which leaves this with
/// nothing to build and only something to check. It borrows in every case and
/// allocates in none.
///
/// Errors on a non-empty pointer without a leading `/`, or a malformed `~`
/// escape.
fn canonical_key(pointer: &str) -> Result<&str, RegistryError> {
    if crate::json_pointer::addresses_root(pointer) {
        // No function can register at the root, so this never matches a
        // callable; normalize to "/" to give the root one spelling here.
        return Ok("/");
    }
    crate::json_pointer::validate_escapes(pointer).map_err(|_| invalid(pointer))?;
    Ok(pointer)
}

/// Normalize a mount prefix: absolute, no trailing separator, empty for the root.
///
/// Shared so that every API in this crate taking a mount agrees on what a given
/// string means — `Router::with_registry` and `RestGateway` in particular, where
/// two different readings of `"/api/v1/"` would be a trap rather than a feature.
/// Normalizing rather than rejecting keeps the mount a convenience argument
/// instead of a fallible one.
pub(crate) fn normalize_mount(prefix: &str) -> String {
    if prefix.is_empty() || prefix == "/" {
        return String::new();
    }
    let absolute = if prefix.starts_with('/') {
        prefix.to_string()
    } else {
        format!("/{prefix}")
    };
    let trimmed = absolute.trim_end_matches('/');
    // All separators: `"///"` is the root, not the empty-key path `"//"`.
    if trimmed.is_empty() {
        String::new()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a call and return the JSON body the handler wrote.
    fn call(registry: &Registry, pointer: &str, params: Option<RequestBody<'_>>) -> String {
        let mut buf = Vec::new();
        let mut out = ResponseBody::new(&mut buf);
        registry
            .call_detached(pointer, params, &mut out)
            .expect("call");
        String::from_utf8(buf).expect("json is utf-8")
    }

    #[test]
    fn a_handler_can_write_its_own_body() {
        // The shape a streaming or typed-slice answer needs, and the reason
        // `WithBody` exists: `out` is the frame's buffer, not a value to encode.
        let registry = Registry::new();
        registry
            .register_function(
                "/samples",
                WithBody(
                    |_params: Option<RequestBody<'_>>, out: &mut ResponseBody<'_>| {
                        out.write_typed_slice(&[1.0f64, 2.0, 3.0]);
                        Ok(())
                    },
                ),
            )
            .expect("register");

        let mut buf = Vec::new();
        let mut out = ResponseBody::new(&mut buf);
        registry
            .call_detached("/samples", None, &mut out)
            .expect("call");
        assert_eq!(out.format(), BodyFormat::Beve);
        assert_eq!(
            structio::from_beve::<Vec<f64>>(&buf).unwrap(),
            vec![1.0, 2.0, 3.0]
        );
    }

    #[test]
    fn a_beve_request_is_answered_in_beve() {
        // The registry used to hand back a value and let the caller encode it,
        // so the format followed the request. It writes into the frame now, and
        // it still has to.
        let registry = Registry::new();
        registry
            .register_function("/echo", |_: Option<RequestBody<'_>>| Ok(7u64))
            .expect("register");

        let mut buf = Vec::new();
        let mut out = ResponseBody::with_format(&mut buf, BodyFormat::Beve);
        registry
            .call_detached("/echo", None, &mut out)
            .expect("call");
        assert_eq!(out.format(), BodyFormat::Beve);
        assert_eq!(structio::from_beve::<u64>(&buf).unwrap(), 7);
    }

    #[test]
    fn a_malformed_pointer_is_refused_at_registration_and_at_lookup() {
        let registry = Registry::new();
        assert!(matches!(
            registry.register_function("/a~2b", |_: Option<RequestBody<'_>>| Ok(0u64)),
            Err(RegistryError::InvalidPointer { .. })
        ));
        assert!(!registry.is_function("/a~2b"));
    }

    #[test]
    fn an_escaped_pointer_registers_and_resolves() {
        // The canonical key is the pointer itself, so an escaped one has to
        // match byte for byte on both sides rather than through a rebuild.
        let registry = Registry::new();
        registry
            .register_function("/a~1b/run", |_: Option<RequestBody<'_>>| Ok(true))
            .expect("register");
        registry
            .register_function("/m~0n", |_: Option<RequestBody<'_>>| Ok(true))
            .expect("register");
        assert!(registry.is_function("/a~1b/run"));
        assert!(registry.is_function("/m~0n"));
    }

    #[test]
    fn a_call_reads_its_parameters_and_writes_its_result() {
        let registry = Registry::new();
        registry
            .register_function("/double", |params: Option<RequestBody<'_>>| {
                let n: i64 = params
                    .ok_or((ErrorCode::InvalidBody, String::from("body required")))?
                    .read("/double")
                    .map_err(|err| (ErrorCode::InvalidBody, err.to_string()))?;
                Ok(n * 2)
            })
            .expect("register");

        let body = RequestBody::new(b"21", BodyFormat::Json);
        assert_eq!(call(&registry, "/double", Some(body)), "42");
    }

    #[test]
    fn a_bodiless_frame_calls_with_no_parameters() {
        let registry = Registry::new();
        registry
            .register_function("/ping", |params: Option<RequestBody<'_>>| {
                assert!(params.is_none());
                Ok("pong")
            })
            .expect("register");

        assert_eq!(call(&registry, "/ping", None), "\"pong\"");
    }

    #[test]
    fn an_unregistered_pointer_is_not_found() {
        let registry = Registry::new();
        let mut buf = Vec::new();
        let mut out = ResponseBody::new(&mut buf);
        let err = registry
            .call_detached("/missing", None, &mut out)
            .unwrap_err();
        assert!(matches!(err, RegistryError::PathNotFound { .. }));
    }

    #[test]
    fn calls_resolve_at_escaped_pointers() {
        // The function map is keyed by the canonical pointer string. A pointer
        // whose reference token contains an escaped `/` (`~1`) or `~` (`~0`)
        // must still register and dispatch: registration stores the canonical
        // key, and dispatch rebuilds the byte-identical key via
        // `canonical_key`'s owned (escape) path. This is the branch the
        // borrow-the-wire-pointer fast path cannot cover.
        let registry = Registry::new();
        registry
            .register_function("/a~1b/run", |_params: Option<RequestBody<'_>>| Ok("slash"))
            .expect("register escaped-slash function");
        registry
            .register_function("/m~0n", |_params: Option<RequestBody<'_>>| Ok("tilde"))
            .expect("register escaped-tilde function");

        assert_eq!(call(&registry, "/a~1b/run", None), "\"slash\"");
        assert_eq!(call(&registry, "/m~0n", None), "\"tilde\"");
        assert!(registry.is_function("/a~1b/run"));

        let mut endpoints = registry.endpoints();
        endpoints.sort();
        assert_eq!(endpoints, vec!["/a~1b/run", "/m~0n"]);
    }

    #[test]
    fn a_context_aware_call_reaches_the_peer() {
        use crate::peer::{NotifyBody, PeerHandle, PeerId, PeerSendError, PeerSink};
        use std::sync::Mutex;

        #[derive(Default)]
        struct CapturingSink {
            captured: Mutex<Vec<String>>,
        }
        impl PeerSink for CapturingSink {
            fn send_notify(&self, method: &str, _body: NotifyBody) -> Result<(), PeerSendError> {
                self.captured.lock().unwrap().push(method.to_string());
                Ok(())
            }
        }

        let sink = Arc::new(CapturingSink::default());
        let peer = PeerHandle::new(PeerId(7), sink.clone());

        let observed_peer = Arc::new(Mutex::new(None::<PeerId>));
        let observed_peer_clone = Arc::clone(&observed_peer);
        let registry = Registry::new();
        registry
            .register_function(
                "/run",
                WithContext(move |ctx: &CallContext, _params: Option<RequestBody<'_>>| {
                    if let Some(p) = ctx.peer() {
                        *observed_peer_clone.lock().unwrap() = Some(p.peer_id());
                        p.send_notify("/progress", NotifyBody::Json(b"{}".to_vec()))
                            .ok();
                    }
                    Ok("ok")
                }),
            )
            .expect("register WithContext function");

        let ctx = CallContext::new("/run", &peer);
        let mut buf = Vec::new();
        let mut out = ResponseBody::new(&mut buf);
        registry.call("/run", None, &ctx, &mut out).expect("call");
        assert_eq!(buf, b"\"ok\"");
        assert_eq!(*observed_peer.lock().unwrap(), Some(PeerId(7)));
        assert_eq!(sink.captured.lock().unwrap().as_slice(), &["/progress"]);
    }

    #[test]
    fn call_detached_uses_a_detached_context() {
        let registry = Registry::new();
        let observed = Arc::new(std::sync::Mutex::new(false));
        let observed_clone = Arc::clone(&observed);
        registry
            .register_function(
                "/probe",
                WithContext(move |ctx: &CallContext, _params: Option<RequestBody<'_>>| {
                    *observed_clone.lock().unwrap() = ctx.peer().is_none();
                    Ok("ok")
                }),
            )
            .expect("register");

        call(&registry, "/probe", None);
        assert!(
            *observed.lock().unwrap(),
            "call_detached must use a detached context"
        );
    }

    #[test]
    fn body_of_borrows_the_frame_under_its_declared_format() {
        let req = Message::builder()
            .id(1)
            .query_str("/name")
            .query_format(crate::constants::QueryFormat::JsonPointer)
            .body_utf8("alice")
            .build();
        let body = Registry::body_of(&req).expect("body").expect("present");
        assert_eq!(body.bytes(), b"alice");
        assert_eq!(body.format(), BodyFormat::Utf8);
    }

    #[test]
    fn body_of_is_none_for_a_bodiless_frame() {
        let req = Message::builder().id(1).query_str("/ping").build();
        assert!(Registry::body_of(&req).expect("body").is_none());
    }
}
