//! Peer-aware handler routing.
//!
//! When a server registers a handler that needs to push more than one message
//! back to the client that called it (for example: server-pushed file chunks
//! after a `/run_collection` request), the handler needs a typed handle to
//! that specific connection. This module provides those types:
//!
//! - [`PeerSink`]: a trait the embedding server implements against its own
//!   per-connection outbound mechanism (a bounded channel, a mutex-guarded
//!   writer, or anything else that serializes pushes for one peer).
//! - [`PeerHandle`]: an `Arc`-wrapped handle to one peer's [`PeerSink`] plus
//!   a server-assigned [`PeerId`].
//! - [`CallContext`]: the value passed to handlers during dispatch so they
//!   can reach the calling peer's [`PeerHandle`] (when the dispatch path
//!   knows about it).
//!
//! The built-in [`WebSocketServer`](crate::websocket_server::WebSocketServer)
//! constructs a [`PeerHandle`] per connection (backed by an internal
//! [`PeerSink`] over its outbound channel) and threads it into each request's
//! [`CallContext`], so context-aware handlers reach the calling peer via
//! [`CallContext::peer`] with no extra wiring. The TCP servers and direct
//! in-process dispatch do not attach a peer: there [`CallContext::peer`]
//! returns `None`, and an embedder that needs peer routing wires its own
//! [`PeerSink`] against its server's outbound channel and calls
//! [`Registry::dispatch_with_ctx`](crate::registry::Registry::dispatch_with_ctx)
//! with a populated [`CallContext`].

use crate::constants::BodyFormat;
use crate::error::RepeError;
use serde::Serialize;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Server-assigned identifier for a connected peer.
///
/// The numeric meaning is the embedder's choice; repe-rs treats it as an
/// opaque tag. Typical implementations use a monotonically increasing
/// counter assigned at accept time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerId(pub u64);

impl PeerId {
    /// Sentinel value used by [`CallContext::detached`] when no peer is
    /// associated with a dispatch.
    pub const DETACHED: PeerId = PeerId(u64::MAX);
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Errors returned by [`PeerHandle::send_notify`].
#[derive(Debug, thiserror::Error)]
pub enum PeerSendError {
    /// The peer's outbound channel is closed (e.g. the peer disconnected).
    #[error("peer disconnected")]
    Disconnected,
    /// The peer's outbound channel is full and the embedder chose to
    /// surface a push failure instead of blocking.
    #[error("peer outbound queue full")]
    Full,
    /// Embedder-specific error. The string carries the embedder's detail.
    #[error("peer send failed: {0}")]
    Other(String),
}

/// Body of an outbound notify pushed via [`PeerHandle::send_notify`].
///
/// The variant carries the wire-format tag so the [`PeerSink`] implementation
/// can populate the resulting REPE message's `body_format` field correctly.
#[derive(Debug, Clone)]
pub enum NotifyBody {
    /// Pre-encoded BEVE bytes.
    Beve(Vec<u8>),
    /// Pre-encoded JSON bytes.
    Json(Vec<u8>),
    /// UTF-8 text.
    Utf8(String),
    /// Raw bytes; the embedder picks `body_format`.
    Raw(Vec<u8>, BodyFormat),
}

impl NotifyBody {
    /// The REPE [`BodyFormat`] that the resulting message should advertise.
    pub fn body_format(&self) -> BodyFormat {
        match self {
            NotifyBody::Beve(_) => BodyFormat::Beve,
            NotifyBody::Json(_) => BodyFormat::Json,
            NotifyBody::Utf8(_) => BodyFormat::Utf8,
            NotifyBody::Raw(_, fmt) => *fmt,
        }
    }

    /// Borrow the body bytes regardless of the variant.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            NotifyBody::Beve(bytes) | NotifyBody::Json(bytes) | NotifyBody::Raw(bytes, _) => bytes,
            NotifyBody::Utf8(text) => text.as_bytes(),
        }
    }

    /// Consume the value and return its bytes regardless of the variant.
    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            NotifyBody::Beve(bytes) | NotifyBody::Json(bytes) | NotifyBody::Raw(bytes, _) => bytes,
            NotifyBody::Utf8(text) => text.into_bytes(),
        }
    }
}

/// Implemented by the embedder for each connected peer's outbound side.
///
/// Repe-rs does not provide a default implementation. Embedders construct
/// their own (typically backed by a bounded channel that a separate writer
/// task drains onto the wire) and wrap it in a [`PeerHandle`].
pub trait PeerSink: Send + Sync {
    /// Push a notify message to the peer.
    ///
    /// `method` is the REPE query (a JSON pointer for `JsonPointer` queries).
    /// The implementation builds the resulting REPE [`Message`](crate::Message),
    /// sets `notify=1`, and writes it to the peer's outbound transport.
    ///
    /// Implementations should be fast and non-blocking when possible. A
    /// bounded channel that returns [`PeerSendError::Full`] when saturated
    /// is the recommended shape; embedders that prefer to block until the
    /// channel has capacity may do so.
    ///
    /// **Query format is currently fixed at `QueryFormat::JsonPointer`.**
    /// The built-in [`WebSocketServer`](crate::websocket_server::WebSocketServer)
    /// sink and the present trait shape both assume `method` is a JSON
    /// pointer; pushing notifies that should advertise a different
    /// query format (custom binary protocols, embedder-defined codes)
    /// requires building the [`Message`](crate::Message) by hand and
    /// writing it through a parallel mechanism. A future revision will
    /// thread a `QueryFormat` through this signature; for now the
    /// limitation is by design, not an oversight.
    fn send_notify(&self, method: &str, body: NotifyBody) -> Result<(), PeerSendError>;

    /// Returns `true` if the peer's transport is still open.
    ///
    /// Default: always `true`. Embedders that can cheaply detect
    /// disconnection should override this so handlers can skip work.
    fn is_connected(&self) -> bool {
        true
    }
}

/// Cloneable handle to one connected peer's outbound sink.
///
/// `PeerHandle`s are constructed by the embedder's connection-accept logic
/// and surfaced to handlers via [`CallContext`].
#[derive(Clone)]
pub struct PeerHandle {
    peer_id: PeerId,
    sink: Arc<dyn PeerSink>,
}

impl PeerHandle {
    /// Build a `PeerHandle` from a peer id and a [`PeerSink`] implementation.
    pub fn new(peer_id: PeerId, sink: Arc<dyn PeerSink>) -> Self {
        Self { peer_id, sink }
    }

    /// Server-assigned identifier for this peer.
    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    /// Push a notify message to this peer. See [`PeerSink::send_notify`].
    pub fn send_notify(&self, method: &str, body: NotifyBody) -> Result<(), PeerSendError> {
        self.sink.send_notify(method, body)
    }

    /// Returns `true` if the peer's transport is still open.
    pub fn is_connected(&self) -> bool {
        self.sink.is_connected()
    }
}

impl std::fmt::Debug for PeerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerHandle")
            .field("peer_id", &self.peer_id)
            .finish()
    }
}

/// Backing source for a [`CallContext`]'s cancellation signal.
///
/// Crate-internal so the public surface stays small and runtime-neutral:
/// the only signal a handler sees is [`CallContext::cancelled`] /
/// [`CallContext::is_cancelled`], never the backing type. The built-in
/// `WebSocketServer` implements this over a `tokio_util` `CancellationToken`
/// that is cancelled when the peer disconnects or the server shuts down;
/// peer-less transports never attach one, so the signal degrades to a
/// never-cancelling no-op (mirroring [`CallContext::peer`] returning
/// `None`).
///
/// Defined here, rather than alongside the WebSocket server, so this
/// transport-agnostic module owns no runtime-specific dependency: the
/// trait is pure `std`, and the concrete `tokio_util` implementation
/// lives behind the `websocket` feature.
pub(crate) trait CancelSignal: Send + Sync {
    /// Non-blocking check: has cancellation fired?
    fn is_cancelled(&self) -> bool;
    /// A future that resolves once cancellation fires. Boxed to keep the
    /// backing future type out of repe's public surface.
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// Per-call dispatch context handed to peer-aware handlers.
///
/// Constructed once per inbound request by whatever code drives dispatch
/// (the embedding server, or a built-in helper if/when repe-rs grows one).
/// Handlers reach the calling peer through [`CallContext::peer`]; methods
/// dispatched without a peer (local round-trip tests, registry batch
/// fixups) get [`CallContext::detached`].
#[derive(Clone, Copy)]
pub struct CallContext<'a> {
    method: &'a str,
    peer: Option<&'a PeerHandle>,
    cancel: Option<&'a dyn CancelSignal>,
}

impl<'a> CallContext<'a> {
    /// Build a context with a peer attached and no cancellation signal.
    pub fn new(method: &'a str, peer: &'a PeerHandle) -> Self {
        Self {
            method,
            peer: Some(peer),
            cancel: None,
        }
    }

    /// Build a context with no peer attached. Handlers that try to push
    /// notifies will see `peer().is_none()` and decide what to do.
    pub fn detached(method: &'a str) -> Self {
        Self {
            method,
            peer: None,
            cancel: None,
        }
    }

    /// Build a context with a peer and a cancellation signal attached.
    /// Used by the built-in `WebSocketServer` to thread the connection's
    /// cancellation handle (fired on disconnect / shutdown) onto each
    /// dispatch.
    // Only the feature-gated WebSocket server constructs a cancel-bearing
    // context (the unit tests above also exercise it); a build without the
    // `websocket` feature has no caller, so don't warn there.
    #[cfg_attr(not(feature = "websocket"), allow(dead_code))]
    pub(crate) fn with_cancel(
        method: &'a str,
        peer: &'a PeerHandle,
        cancel: &'a dyn CancelSignal,
    ) -> Self {
        Self {
            method,
            peer: Some(peer),
            cancel: Some(cancel),
        }
    }

    /// The query path this dispatch is targeting (e.g. `/run_collection`).
    pub fn method(&self) -> &'a str {
        self.method
    }

    /// The calling peer, if known.
    pub fn peer(&self) -> Option<&'a PeerHandle> {
        self.peer
    }

    /// Non-blocking check of whether this call should stop: the peer has
    /// disconnected, or the server is shutting down.
    ///
    /// A long off-reader handler (one registered via
    /// `Router::with_*_blocking`) should poll this at loop boundaries and
    /// return early once it reads `true`, freeing its blocking-pool
    /// thread instead of running pointless work to completion. Always
    /// `false` on peer-less transports (TCP servers, in-process
    /// dispatch), which never attach a cancellation signal.
    ///
    /// This complements, and does not replace, the
    /// [`on_peer_disconnect`](crate::PeerRegistry) →
    /// [`TransferControl::cancel`](crate::stream::TransferControl::cancel)
    /// path: a producer parked in
    /// [`wait_for_credit`](crate::stream::TransferControl::wait_for_credit)
    /// is woken only by `cancel`, not by this signal.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_some_and(|c| c.is_cancelled())
    }

    /// Resolves when this call should stop (peer disconnected or server
    /// shutting down). Never resolves on peer-less transports, so a
    /// `select!` arm built on it stays dormant there rather than firing
    /// spuriously.
    ///
    /// Intended for an async handler to `select!` on; a synchronous
    /// off-reader handler should poll [`is_cancelled`](Self::is_cancelled)
    /// at loop boundaries instead.
    pub fn cancelled(&self) -> impl Future<Output = ()> + Send + 'a {
        // Copy the signal reference out so the returned future is tied to
        // `'a` (the signal's lifetime), not to the `&self` borrow.
        let cancel = self.cancel;
        async move {
            match cancel {
                Some(c) => c.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        }
    }
}

impl std::fmt::Debug for CallContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallContext")
            .field("method", &self.method)
            .field("peer", &self.peer)
            .field("cancellable", &self.cancel.is_some())
            .finish()
    }
}

/// Live set of peers attached to one server instance.
///
/// Inserted on accept, removed on close. Cheap to clone (`Arc` inside)
/// so background tasks can hold a handle for broadcasting. The registry
/// is transport-agnostic: anything that produces [`PeerHandle`]s can
/// feed it, not just the built-in `WebSocketServer`.
///
/// Typical wiring:
///
/// ```ignore
/// let peers = PeerRegistry::new();
/// let server = WebSocketServer::new(router).with_peer_registry(peers.clone());
///
/// let publisher = peers.clone();
/// tokio::spawn(async move {
///     publisher.broadcast_notify_json("/state/changed", &snapshot);
/// });
/// ```
#[derive(Clone)]
pub struct PeerRegistry {
    inner: Arc<Mutex<HashMap<PeerId, PeerHandle>>>,
    next_id: Arc<AtomicU64>,
}

impl Default for PeerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerRegistry {
    /// Build an empty registry.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Mint a fresh, monotonically increasing [`PeerId`] from this
    /// registry's id counter.
    ///
    /// Embedders that build their own [`PeerHandle`]s (rather than
    /// going through `WebSocketServer::with_peer_registry`) can call
    /// this to keep ids unique across every server feeding the
    /// registry. `WebSocketServer::with_peer_registry` adopts this
    /// counter internally, so two servers sharing one registry never
    /// mint colliding ids.
    pub fn next_peer_id(&self) -> PeerId {
        let value = self.next_id.fetch_add(1, Ordering::Relaxed);
        debug_assert!(
            value != PeerId::DETACHED.0,
            "PeerRegistry id counter collided with PeerId::DETACHED sentinel \
             (this requires ~2^64 connections; debug-only tripwire, not a contract)"
        );
        PeerId(value)
    }

    /// Internal counter handle shared with `WebSocketServer` so that
    /// all servers wired to one registry mint unique ids.
    #[cfg(all(feature = "websocket", not(target_arch = "wasm32")))]
    pub(crate) fn id_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.next_id)
    }

    /// Current number of connected peers.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// `true` if no peers are connected.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Snapshot of currently-connected peer handles.
    ///
    /// The registry lock is held only long enough to clone the values
    /// into the returned `Vec`; callers can iterate freely without
    /// blocking inserts or removals.
    pub fn peers(&self) -> Vec<PeerHandle> {
        self.lock().values().cloned().collect()
    }

    /// Look up a peer by id. Returns `None` if the peer has
    /// disconnected.
    pub fn get(&self, id: PeerId) -> Option<PeerHandle> {
        self.lock().get(&id).cloned()
    }

    /// Insert a peer.
    ///
    /// Callers are responsible for ensuring `PeerId`s are unique
    /// within one registry. `WebSocketServer::with_peer_registry`
    /// already does this for you (it adopts the registry's id
    /// counter); embedders rolling their own pipeline should mint ids
    /// via [`PeerRegistry::next_peer_id`]. Overwriting an existing
    /// entry trips a `debug_assert!` so the silent-corruption mode
    /// (two servers minting `PeerId(0)` against a shared registry)
    /// fires loudly in tests.
    pub fn insert(&self, peer: PeerHandle) {
        let prev = self.lock().insert(peer.peer_id(), peer);
        debug_assert!(
            prev.is_none(),
            "PeerRegistry::insert overwrote an existing peer; \
             two PeerHandles minted the same PeerId. If you wired multiple \
             WebSocketServers to one PeerRegistry without using with_peer_registry, \
             share PeerRegistry::next_peer_id across them."
        );
    }

    /// Remove a peer. Returns the removed handle if it was present.
    pub fn remove(&self, id: PeerId) -> Option<PeerHandle> {
        self.lock().remove(&id)
    }

    /// Broadcast a JSON-bodied notify to every connected peer.
    ///
    /// The body is encoded once on the caller's task; the encoded
    /// bytes are cloned into each peer's outbound channel. Returns a
    /// per-peer result map so callers can surface backpressure
    /// ([`PeerSendError::Full`]) or prune dead peers
    /// ([`PeerSendError::Disconnected`]). The registry's lock is held
    /// only long enough to snapshot the peer map; per-peer sends run
    /// outside the lock, so a slow peer cannot stall delivery to the
    /// others.
    pub fn broadcast_notify_json<P, T>(
        &self,
        path: P,
        body: &T,
    ) -> Result<HashMap<PeerId, Result<(), PeerSendError>>, RepeError>
    where
        P: AsRef<str>,
        T: Serialize + ?Sized,
    {
        let encoded = serde_json::to_vec(body)?;
        Ok(self.broadcast_each(path.as_ref(), |_peer| NotifyBody::Json(encoded.clone())))
    }

    /// BEVE-bodied broadcast. See [`broadcast_notify_json`] for
    /// semantics.
    ///
    /// [`broadcast_notify_json`]: PeerRegistry::broadcast_notify_json
    pub fn broadcast_notify_beve<P, T>(
        &self,
        path: P,
        body: &T,
    ) -> Result<HashMap<PeerId, Result<(), PeerSendError>>, RepeError>
    where
        P: AsRef<str>,
        T: Serialize,
    {
        let encoded = beve::to_vec(body)?;
        Ok(self.broadcast_each(path.as_ref(), |_peer| NotifyBody::Beve(encoded.clone())))
    }

    /// UTF-8-bodied broadcast. Plain text is advertised with
    /// [`BodyFormat::Utf8`].
    pub fn broadcast_notify_utf8<P, S>(
        &self,
        path: P,
        text: S,
    ) -> HashMap<PeerId, Result<(), PeerSendError>>
    where
        P: AsRef<str>,
        S: AsRef<str>,
    {
        let text = text.as_ref().to_owned();
        self.broadcast_each(path.as_ref(), |_peer| NotifyBody::Utf8(text.clone()))
    }

    /// Raw-body broadcast. The caller supplies the body bytes and the
    /// [`BodyFormat`] tag the wire should advertise.
    pub fn broadcast_notify_raw<P>(
        &self,
        path: P,
        body_format: BodyFormat,
        body: &[u8],
    ) -> HashMap<PeerId, Result<(), PeerSendError>>
    where
        P: AsRef<str>,
    {
        let bytes = body.to_vec();
        self.broadcast_each(path.as_ref(), move |_peer| {
            NotifyBody::Raw(bytes.clone(), body_format)
        })
    }

    fn broadcast_each<F>(
        &self,
        path: &str,
        mut body_for: F,
    ) -> HashMap<PeerId, Result<(), PeerSendError>>
    where
        F: FnMut(&PeerHandle) -> NotifyBody,
    {
        let snapshot = self.peers();
        let mut out = HashMap::with_capacity(snapshot.len());
        for peer in snapshot {
            let body = body_for(&peer);
            let result = peer.send_notify(path, body);
            out.insert(peer.peer_id(), result);
        }
        out
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PeerId, PeerHandle>> {
        // Recover from poison: the map's invariants (an `Arc`-keyed
        // `HashMap<PeerId, PeerHandle>` with no cross-entry coupling)
        // cannot be left inconsistent by any single insert/remove, so
        // honoring the poison would block the registry forever on the
        // strength of an unrelated panic in some other code path.
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl std::fmt::Debug for PeerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerRegistry")
            .field("len", &self.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct CapturingSink {
        sent: Mutex<Vec<(String, Vec<u8>, BodyFormat)>>,
        connected: bool,
    }

    impl PeerSink for CapturingSink {
        fn send_notify(&self, method: &str, body: NotifyBody) -> Result<(), PeerSendError> {
            if !self.connected {
                return Err(PeerSendError::Disconnected);
            }
            let fmt = body.body_format();
            self.sent
                .lock()
                .unwrap()
                .push((method.to_string(), body.into_bytes(), fmt));
            Ok(())
        }

        fn is_connected(&self) -> bool {
            self.connected
        }
    }

    #[test]
    fn peer_handle_sends_through_sink() {
        let sink = Arc::new(CapturingSink {
            connected: true,
            ..Default::default()
        });
        let peer = PeerHandle::new(PeerId(7), sink.clone());
        peer.send_notify("/x", NotifyBody::Beve(vec![1, 2, 3]))
            .unwrap();
        peer.send_notify("/y", NotifyBody::Utf8("hi".into()))
            .unwrap();
        let captured = sink.sent.lock().unwrap();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].0, "/x");
        assert_eq!(captured[0].1, vec![1, 2, 3]);
        assert_eq!(captured[0].2, BodyFormat::Beve);
        assert_eq!(captured[1].0, "/y");
        assert_eq!(captured[1].1, b"hi".to_vec());
        assert_eq!(captured[1].2, BodyFormat::Utf8);
        assert_eq!(peer.peer_id(), PeerId(7));
        assert!(peer.is_connected());
    }

    #[test]
    fn peer_handle_propagates_disconnect() {
        let sink = Arc::new(CapturingSink::default());
        let peer = PeerHandle::new(PeerId(1), sink);
        let err = peer
            .send_notify("/z", NotifyBody::Json(b"{}".to_vec()))
            .unwrap_err();
        assert!(matches!(err, PeerSendError::Disconnected));
        assert!(!peer.is_connected());
    }

    #[test]
    fn call_context_constructors() {
        let sink = Arc::new(CapturingSink {
            connected: true,
            ..Default::default()
        });
        let peer = PeerHandle::new(PeerId(2), sink);
        let with = CallContext::new("/m", &peer);
        assert_eq!(with.method(), "/m");
        assert!(with.peer().is_some());

        let without = CallContext::detached("/m");
        assert_eq!(without.method(), "/m");
        assert!(without.peer().is_none());
    }

    struct MockSignal(bool);
    impl CancelSignal for MockSignal {
        fn is_cancelled(&self) -> bool {
            self.0
        }
        fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            if self.0 {
                Box::pin(std::future::ready(()))
            } else {
                Box::pin(std::future::pending())
            }
        }
    }

    #[test]
    fn call_context_cancellation_reflects_signal() {
        let sink = Arc::new(CapturingSink {
            connected: true,
            ..Default::default()
        });
        let peer = PeerHandle::new(PeerId(3), sink);

        let cancelled = MockSignal(true);
        let ctx = CallContext::with_cancel("/m", &peer, &cancelled);
        assert!(ctx.is_cancelled());
        assert!(ctx.peer().is_some());

        let live = MockSignal(false);
        let ctx = CallContext::with_cancel("/m", &peer, &live);
        assert!(!ctx.is_cancelled());
    }

    #[test]
    fn detached_and_plain_contexts_never_cancel() {
        // Peer-less and signal-less contexts degrade to a never-cancelling
        // no-op, mirroring `peer()` returning `None`.
        let sink = Arc::new(CapturingSink {
            connected: true,
            ..Default::default()
        });
        let peer = PeerHandle::new(PeerId(4), sink);
        assert!(!CallContext::detached("/m").is_cancelled());
        assert!(!CallContext::new("/m", &peer).is_cancelled());
    }
}
